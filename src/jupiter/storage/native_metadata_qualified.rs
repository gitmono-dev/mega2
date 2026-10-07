//! Qualified metadata incarnations. Runtime serving adoption is intentionally closed.

use std::ops::Deref;

use super::*;

pub struct PostgresQualifiedMetadataRepository {
    inner: PostgresMetadataGenerationRepository,
}

impl Deref for PostgresQualifiedMetadataRepository {
    type Target = PostgresMetadataGenerationRepository;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataLifetime {
    page_id: [u8; 32],
    generation: i64,
    metadata_codec: i16,
    expected_size: i32,
    primary_scope: Box<[u8]>,
}

impl MetadataLifetime {
    pub fn page_id(&self) -> [u8; 32] {
        self.page_id
    }

    pub fn generation(&self) -> i64 {
        self.generation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataGcPhase {
    Claim,
    Apply,
}

#[derive(Debug, thiserror::Error)]
pub enum MetadataGcError {
    #[error(transparent)]
    Rejected(#[from] SnapshotError),
    #[error("metadata GC {phase:?} commit outcome is unknown for {operation_id}")]
    CommitUncertain {
        operation_id: String,
        page_id: [u8; 32],
        generation: i64,
        phase: MetadataGcPhase,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataGcClaim {
    operation_id: String,
    lifetime: MetadataLifetime,
    graph_present: bool,
    had_payload: bool,
}

impl MetadataGcClaim {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn lifetime(&self) -> &MetadataLifetime {
        &self.lifetime
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataGcReceipt {
    claim: MetadataGcClaim,
    created_at: sea_orm::prelude::DateTimeWithTimeZone,
    completed_at: sea_orm::prelude::DateTimeWithTimeZone,
}

impl MetadataGcReceipt {
    pub fn claim(&self) -> &MetadataGcClaim {
        &self.claim
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataGcObservation {
    Absent,
    Pending(Box<MetadataGcClaim>),
    Applied(Box<MetadataGcReceipt>),
}

struct GcRecord {
    claim: MetadataGcClaim,
    state: String,
    created_at: sea_orm::prelude::DateTimeWithTimeZone,
    completed_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

impl GcRecord {
    fn receipt(self) -> Result<MetadataGcReceipt, SnapshotError> {
        if self.state != "APPLIED" {
            return Err(unavailable("metadata GC operation is not APPLIED"));
        }
        Ok(MetadataGcReceipt {
            claim: self.claim,
            created_at: self.created_at,
            completed_at: self
                .completed_at
                .ok_or_else(|| integrity("APPLIED metadata GC receipt has no timestamp"))?,
        })
    }
}

impl PostgresQualifiedMetadataRepository {
    pub async fn new(connection: DatabaseConnection) -> Result<Self, SnapshotError> {
        Ok(Self {
            inner: PostgresMetadataGenerationRepository {
                inner: PostgresMetadataInstallRepository::new(connection).await?,
                graph_domain: "qualified-v1",
            },
        })
    }

    pub async fn lifetime(&self, page_id: [u8; 32]) -> Result<MetadataLifetime, SnapshotError> {
        self.inner
            .inner
            .verify_primary_connection(&self.inner.inner.connection)
            .await?;
        let row = self
            .inner
            .inner
            .connection
            .query_one_raw(statement(
                "SELECT l.generation,l.metadata_codec,l.expected_size FROM mst2_metadata_current c
             JOIN mst2_metadata_lifetime l USING(page_id,generation)
             WHERE c.page_id=$1 AND l.graph_domain='qualified-v1'",
                [page_id.to_vec().into()],
            ))
            .await
            .map_err(internal)?
            .ok_or_else(|| unavailable("qualified lifetime is missing"))?;
        Ok(MetadataLifetime {
            page_id,
            generation: row.try_get("", "generation").map_err(internal)?,
            metadata_codec: row.try_get("", "metadata_codec").map_err(internal)?,
            expected_size: row.try_get("", "expected_size").map_err(internal)?,
            primary_scope: self.inner.primary_scope()?.into_boxed_slice(),
        })
    }

    /// Read-only recovery selector; claim still requires this tuple to be current.
    pub async fn historical_lifetime(
        &self,
        page_id: [u8; 32],
        generation: i64,
    ) -> Result<MetadataLifetime, SnapshotError> {
        if generation <= 0 {
            return Err(integrity("historical generation must be positive"));
        }
        self.inner
            .inner
            .verify_primary_connection(&self.inner.inner.connection)
            .await?;
        let row = self
            .inner
            .inner
            .connection
            .query_one_raw(statement(
                "SELECT metadata_codec,expected_size FROM mst2_metadata_lifetime
             WHERE page_id=$1 AND generation=$2 AND graph_domain='qualified-v1'",
                [page_id.to_vec().into(), generation.into()],
            ))
            .await
            .map_err(internal)?
            .ok_or_else(|| unavailable("qualified historical incarnation is missing"))?;
        Ok(MetadataLifetime {
            page_id,
            generation,
            metadata_codec: row.try_get("", "metadata_codec").map_err(internal)?,
            expected_size: row.try_get("", "expected_size").map_err(internal)?,
            primary_scope: self.inner.primary_scope()?.into_boxed_slice(),
        })
    }

    fn require_lifetime_scope(&self, lifetime: &MetadataLifetime) -> Result<(), SnapshotError> {
        if lifetime.primary_scope.as_ref() != self.inner.primary_scope()?.as_slice() {
            return Err(integrity(
                "metadata GC belongs to another captured primary scope",
            ));
        }
        Ok(())
    }

    /// Bounded indexed discovery only. Every returned tuple still needs a full claim proof.
    pub async fn scan_current_lifetimes(
        &self,
        after: Option<([u8; 32], i64)>,
        limit: u16,
    ) -> Result<Vec<MetadataLifetime>, SnapshotError> {
        if !(1..=256).contains(&limit) {
            return Err(integrity("metadata scan limit must be 1..=256"));
        }
        self.inner
            .inner
            .verify_primary_connection(&self.inner.inner.connection)
            .await?;
        let (page, generation) = after.map(|(p, g)| (p.to_vec(), g)).unwrap_or_default();
        let rows=self.inner.inner.connection.query_all_raw(statement(
            "SELECT l.page_id,l.generation,l.metadata_codec,l.expected_size FROM mst2_metadata_lifetime l
             JOIN mst2_metadata_current c USING(page_id,generation)
             WHERE l.graph_domain='qualified-v1' AND l.state IN ('RESERVED','LIVE') AND (l.page_id,l.generation)>($1,$2)
             ORDER BY l.page_id,l.generation LIMIT $3",
            [page.into(),generation.into(),i64::from(limit).into()],
        )).await.map_err(internal)?;
        let scope = self.inner.primary_scope()?.into_boxed_slice();
        rows.into_iter()
            .map(|row| {
                let page: Vec<u8> = row.try_get("", "page_id").map_err(internal)?;
                Ok(MetadataLifetime {
                    page_id: page.as_slice().try_into().map_err(internal)?,
                    generation: row.try_get("", "generation").map_err(internal)?,
                    metadata_codec: row.try_get("", "metadata_codec").map_err(internal)?,
                    expected_size: row.try_get("", "expected_size").map_err(internal)?,
                    primary_scope: scope.clone(),
                })
            })
            .collect()
    }

    pub async fn claim(
        &self,
        operation_id: &str,
        lifetime: &MetadataLifetime,
    ) -> Result<MetadataGcClaim, MetadataGcError> {
        validate_gc_operation(operation_id)?;
        self.require_lifetime_scope(lifetime)?;
        let txn = self.inner.inner.transaction().await?;
        let result = self.claim_in_txn(&txn, operation_id, lifetime).await;
        finish_gc_transaction(txn, result, operation_id, lifetime, MetadataGcPhase::Claim).await
    }

    async fn claim_in_txn(
        &self,
        txn: &DatabaseTransaction,
        operation_id: &str,
        lifetime: &MetadataLifetime,
    ) -> Result<MetadataGcClaim, SnapshotError> {
        self.require_lifetime_scope(lifetime)?;
        self.inner.inner.barrier(txn).await?;
        if let Some(record) = load_gc(txn, operation_id).await? {
            if &record.claim.lifetime != lifetime {
                return Err(integrity(
                    "GC operation cannot be retargeted to another lifetime",
                ));
            }
            return Ok(record.claim);
        }
        let row=txn.query_one_raw(statement(
            "SELECT EXISTS(SELECT 1 FROM mst2_metadata_graph_node n WHERE n.page_id=l.page_id AND n.generation=l.generation) AS graph_present,
             EXISTS(SELECT 1 FROM mst2_metadata_payload b WHERE b.page_id=l.page_id) AS had_payload
             FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l USING(page_id,generation)
             WHERE l.page_id=$1 AND l.generation=$2 AND l.graph_domain='qualified-v1'
               AND l.metadata_codec=$3 AND l.expected_size=$4 FOR UPDATE OF c,l",
            [lifetime.page_id.to_vec().into(),lifetime.generation.into(),lifetime.metadata_codec.into(),lifetime.expected_size.into()],
        )).await.map_err(internal)?.ok_or_else(|| unavailable("GC candidate no longer names its captured current incarnation"))?;
        let graph_present: bool = row.try_get("", "graph_present").map_err(internal)?;
        let had_payload: bool = row.try_get("", "had_payload").map_err(internal)?;
        txn.execute_raw(statement(
            "INSERT INTO mst2_metadata_gc_op(operation_id,page_id,generation,primary_scope,graph_domain,
             metadata_codec,expected_size,graph_present,had_payload,state)
             VALUES($1::uuid,$2,$3,$4,'qualified-v1',$5,$6,$7,$8,'PENDING')",
            [operation_id.into(),lifetime.page_id.to_vec().into(),lifetime.generation.into(),
             lifetime.primary_scope.to_vec().into(),lifetime.metadata_codec.into(),lifetime.expected_size.into(),
             graph_present.into(),had_payload.into()],
        )).await.map_err(internal)?;
        Ok(load_gc(txn, operation_id)
            .await?
            .ok_or_else(|| integrity("claimed GC operation disappeared"))?
            .claim)
    }

    pub async fn apply(
        &self,
        claim: &MetadataGcClaim,
    ) -> Result<MetadataGcReceipt, MetadataGcError> {
        self.require_lifetime_scope(&claim.lifetime)?;
        let txn = self.inner.inner.transaction().await?;
        let result = self.apply_in_txn(&txn, claim).await;
        finish_gc_transaction(
            txn,
            result,
            &claim.operation_id,
            &claim.lifetime,
            MetadataGcPhase::Apply,
        )
        .await
    }

    async fn apply_in_txn(
        &self,
        txn: &DatabaseTransaction,
        claim: &MetadataGcClaim,
    ) -> Result<MetadataGcReceipt, SnapshotError> {
        self.require_lifetime_scope(&claim.lifetime)?;
        self.inner.inner.barrier(txn).await?;
        let record = load_gc(txn, &claim.operation_id)
            .await?
            .ok_or_else(|| unavailable("metadata GC operation is missing"))?;
        if &record.claim != claim {
            return Err(integrity(
                "metadata GC claim differs from immutable operation",
            ));
        }
        if record.state == "APPLIED" {
            return record.receipt();
        }
        txn.query_one_raw(statement(
            "SELECT mst2_metadata_gc_apply($1::uuid)",
            [claim.operation_id.clone().into()],
        ))
        .await
        .map_err(internal)?;
        load_gc(txn, &claim.operation_id)
            .await?
            .ok_or_else(|| integrity("applied metadata GC operation disappeared"))?
            .receipt()
    }

    /// Absence is definitive only after a fresh same-primary completion barrier.
    pub async fn inspect_gc(
        &self,
        fresh_primary: &DatabaseConnection,
        operation_id: &str,
        lifetime: &MetadataLifetime,
        phase: MetadataGcPhase,
    ) -> Result<MetadataGcObservation, MetadataGcError> {
        validate_gc_operation(operation_id)?;
        self.require_lifetime_scope(lifetime)?;
        let txn = fresh_primary
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(|_| gc_uncertain(operation_id, lifetime, phase))?;
        if self.inner.inner.barrier(&txn).await.is_err() {
            let _ = txn.rollback().await;
            return Err(gc_uncertain(operation_id, lifetime, phase));
        }
        let result = async {
            let Some(record) = load_gc(&txn, operation_id).await? else {
                return Ok(MetadataGcObservation::Absent);
            };
            if &record.claim.lifetime != lifetime {
                return Err(integrity(
                    "GC inspection differs from its captured lifetime",
                ));
            }
            match record.state.as_str() {
                "PENDING" => Ok(MetadataGcObservation::Pending(Box::new(record.claim))),
                "APPLIED" => Ok(MetadataGcObservation::Applied(Box::new(record.receipt()?))),
                _ => Err(integrity("unknown metadata GC operation state")),
            }
        }
        .await;
        txn.rollback()
            .await
            .map_err(|_| gc_uncertain(operation_id, lifetime, phase))?;
        result.map_err(|error: SnapshotError| {
            if error.code == SnapshotErrorCode::Internal {
                gc_uncertain(operation_id, lifetime, phase)
            } else {
                MetadataGcError::Rejected(error)
            }
        })
    }

    pub async fn pending_gc(&self, limit: u16) -> Result<Vec<MetadataGcClaim>, SnapshotError> {
        if !(1..=256).contains(&limit) {
            return Err(integrity("metadata pending GC limit must be 1..=256"));
        }
        self.inner
            .inner
            .verify_primary_connection(&self.inner.inner.connection)
            .await?;
        let rows=self.inner.inner.connection.query_all_raw(statement(
            "SELECT operation_id::text,page_id,generation,primary_scope,metadata_codec,expected_size,
             graph_present,had_payload,state,created_at,completed_at FROM mst2_metadata_gc_op WHERE state='PENDING'
             ORDER BY created_at,operation_id LIMIT $1",
            [i64::from(limit).into()],
        )).await.map_err(internal)?;
        let mut claims = Vec::with_capacity(rows.len());
        for row in rows {
            let record = gc_record(row)?;
            self.require_lifetime_scope(&record.claim.lifetime)?;
            claims.push(record.claim);
        }
        Ok(claims)
    }

    pub async fn begin_fresh_intent(
        &self,
        operation_id: &str,
        prepared: &PreparedNativeMetadataRetention,
        applied: &[MetadataGcReceipt],
    ) -> Result<GenerationPrepareIntent, MetadataInstallError> {
        if applied.is_empty() || applied.len() > MetadataDagLimits::default().nodes {
            return Err(integrity("fresh begin requires bounded exact APPLIED receipts").into());
        }
        for receipt in applied {
            self.require_lifetime_scope(&receipt.claim.lifetime)?;
        }
        self.inner
            .begin_with_reopens(operation_id, prepared, applied)
            .await
    }
}

fn validate_gc_operation(operation: &str) -> Result<(), SnapshotError> {
    let uuid = uuid::Uuid::parse_str(operation).map_err(internal)?;
    if uuid.get_version_num() != 4 || uuid.to_string() != operation {
        return Err(integrity("metadata GC operation must be canonical UUID v4"));
    }
    Ok(())
}

fn gc_uncertain(
    operation: &str,
    lifetime: &MetadataLifetime,
    phase: MetadataGcPhase,
) -> MetadataGcError {
    MetadataGcError::CommitUncertain {
        operation_id: operation.into(),
        page_id: lifetime.page_id,
        generation: lifetime.generation,
        phase,
    }
}

async fn finish_gc_transaction<T>(
    txn: DatabaseTransaction,
    result: Result<T, SnapshotError>,
    operation: &str,
    lifetime: &MetadataLifetime,
    phase: MetadataGcPhase,
) -> Result<T, MetadataGcError> {
    match result {
        Ok(value) => {
            txn.commit()
                .await
                .map_err(|_| gc_uncertain(operation, lifetime, phase))?;
            Ok(value)
        }
        Err(error) => {
            txn.rollback().await.map_err(internal)?;
            Err(error.into())
        }
    }
}

async fn load_gc<C: ConnectionTrait>(
    connection: &C,
    operation: &str,
) -> Result<Option<GcRecord>, SnapshotError> {
    let Some(row)=connection.query_one_raw(statement(
        "SELECT operation_id::text,page_id,generation,primary_scope,metadata_codec,expected_size,
         graph_present,had_payload,state,created_at,completed_at FROM mst2_metadata_gc_op WHERE operation_id=$1::uuid",
        [operation.into()],
    )).await.map_err(internal)? else { return Ok(None); };
    Ok(Some(gc_record(row)?))
}

fn gc_record(row: QueryResult) -> Result<GcRecord, SnapshotError> {
    let page: Vec<u8> = row.try_get("", "page_id").map_err(internal)?;
    Ok(GcRecord {
        claim: MetadataGcClaim {
            operation_id: row.try_get("", "operation_id").map_err(internal)?,
            lifetime: MetadataLifetime {
                page_id: page.as_slice().try_into().map_err(internal)?,
                generation: row.try_get("", "generation").map_err(internal)?,
                metadata_codec: row.try_get("", "metadata_codec").map_err(internal)?,
                expected_size: row.try_get("", "expected_size").map_err(internal)?,
                primary_scope: row
                    .try_get::<Vec<u8>>("", "primary_scope")
                    .map_err(internal)?
                    .into_boxed_slice(),
            },
            graph_present: row.try_get("", "graph_present").map_err(internal)?,
            had_payload: row.try_get("", "had_payload").map_err(internal)?,
        },
        state: row.try_get("", "state").map_err(internal)?,
        created_at: row.try_get("", "created_at").map_err(internal)?,
        completed_at: row.try_get("", "completed_at").map_err(internal)?,
    })
}

async fn validate_applied_receipts<C: ConnectionTrait>(
    connection: &C,
    receipts: &[MetadataGcReceipt],
) -> Result<(), SnapshotError> {
    if receipts.is_empty() {
        return Ok(());
    }
    let operations: Vec<_> = receipts
        .iter()
        .map(|r| r.claim.operation_id.as_str())
        .collect();
    let rows=connection.query_all_raw(statement(
        "SELECT operation_id::text,page_id,generation,primary_scope,metadata_codec,expected_size,
         graph_present,had_payload,state,created_at,completed_at FROM mst2_metadata_gc_op
         WHERE operation_id IN (SELECT jsonb_array_elements_text($1::jsonb)::uuid) LIMIT 4097",
        [serde_json::to_string(&operations).map_err(internal)?.into()],
    )).await.map_err(internal)?;
    let mut actual = BTreeMap::new();
    for row in rows {
        let record = gc_record(row)?;
        actual.insert(record.claim.operation_id.clone(), record.receipt()?);
    }
    if actual.len() != receipts.len()
        || receipts
            .iter()
            .any(|r| actual.get(&r.claim.operation_id) != Some(r))
    {
        return Err(integrity(
            "fresh proof differs from exact immutable APPLIED operation and timestamps",
        ));
    }
    Ok(())
}

pub(super) async fn reopen_in_txn(
    txn: &DatabaseTransaction,
    plan: &MetadataInstallPlan,
    receipts: &[MetadataGcReceipt],
) -> Result<(), SnapshotError> {
    let mut pages = BTreeSet::new();
    validate_applied_receipts(txn, receipts).await?;
    for receipt in receipts {
        let lifetime = &receipt.claim.lifetime;
        if !pages.insert(lifetime.page_id)
            || plan.pages.get(&lifetime.page_id).copied() != Some(lifetime.expected_size as u64)
            || plan.identity.metadata_codec as i16 != lifetime.metadata_codec
        {
            return Err(integrity(
                "fresh receipts are not unique exact members of the new plan",
            ));
        }
        lifetime
            .generation
            .checked_add(1)
            .ok_or_else(|| unavailable("metadata generation watermark exhausted"))?;
    }
    let operations: Vec<_> = receipts
        .iter()
        .map(|r| r.claim.operation_id.as_str())
        .collect();
    let encoded = serde_json::to_string(&operations).map_err(internal)?;
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size,graph_domain)
         SELECT o.page_id,'page:sha256:'||encode(o.page_id,'hex'),o.generation+1,'RESERVED',o.metadata_codec,o.expected_size,'qualified-v1'
         FROM mst2_metadata_gc_op o WHERE operation_id IN (SELECT jsonb_array_elements_text($1::jsonb)::uuid)
         ORDER BY o.page_id ON CONFLICT(page_id,generation) DO NOTHING",
        [encoded.clone().into()],
    )).await.map_err(internal)?;
    let changed=txn.execute_raw(statement(
        "UPDATE mst2_metadata_current c SET generation=o.generation+1 FROM mst2_metadata_gc_op o
         WHERE o.operation_id IN (SELECT jsonb_array_elements_text($1::jsonb)::uuid)
           AND c.page_id=o.page_id AND c.generation=o.generation",
        [encoded.into()],
    )).await.map_err(internal)?;
    if changed.rows_affected() != receipts.len() as u64 {
        return Err(unavailable(
            "fresh receipts do not name all current removed incarnations",
        ));
    }
    Ok(())
}

pub(super) async fn verify_reopen_replay<C: ConnectionTrait>(
    connection: &C,
    fixed: &FixedPlan,
    receipts: &[MetadataGcReceipt],
) -> Result<(), SnapshotError> {
    validate_applied_receipts(connection, receipts).await?;
    let mut pages = BTreeSet::new();
    for receipt in receipts {
        let old = &receipt.claim.lifetime;
        if !pages.insert(old.page_id)
            || old.primary_scope != fixed.intent.primary_scope
            || old.generation.checked_add(1) != fixed.bindings.0.get(&old.page_id).map(|b| b.0)
            || Some(old.expected_size as u64) != fixed.bindings.0.get(&old.page_id).map(|b| b.1)
        {
            return Err(integrity(
                "fresh replay receipts differ from the fixed new incarnation bindings",
            ));
        }
    }
    Ok(())
}

pub(super) async fn allocate_lifetimes(
    txn: &DatabaseTransaction,
    plan: &MetadataInstallPlan,
) -> Result<GenerationBindings, SnapshotError> {
    let pages: Vec<_> = plan
        .pages
        .iter()
        .map(|(id, size)| json!({"id":hex::encode(id),"size":size}))
        .collect();
    let encoded = serde_json::to_string(&pages).map_err(internal)?;
    if txn.query_one_raw(statement(
        "SELECT l.page_id FROM jsonb_to_recordset($1::jsonb) p(id text,size integer)
         JOIN mst2_metadata_current c ON c.page_id=decode(p.id,'hex')
         JOIN mst2_metadata_lifetime l USING(page_id,generation) WHERE l.graph_domain<>'qualified-v1' LIMIT 1",
        [encoded.clone().into()],
    )).await.map_err(internal)?.is_some() {
        return Err(unavailable("metadata incarnation permanently belongs to another graph domain"));
    }
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size,graph_domain)
         SELECT decode(p.id,'hex'),'page:sha256:'||p.id,1,'RESERVED',$1,p.size,'qualified-v1'
         FROM jsonb_to_recordset($2::jsonb) p(id text,size integer)
         WHERE NOT EXISTS(SELECT 1 FROM mst2_metadata_current c WHERE c.page_id=decode(p.id,'hex'))
         ON CONFLICT(page_id,generation) DO NOTHING",
        [(plan.identity.metadata_codec as i16).into(),encoded.clone().into()],
    )).await.map_err(internal)?;
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_current(page_id,generation) SELECT decode(p.id,'hex'),1
         FROM jsonb_to_recordset($1::jsonb) p(id text,size integer)
         WHERE NOT EXISTS(SELECT 1 FROM mst2_metadata_current c WHERE c.page_id=decode(p.id,'hex'))",
        [encoded.clone().into()],
    )).await.map_err(internal)?;
    let rows=txn.query_all_raw(statement(
        "SELECT l.page_id,l.generation,l.state,l.graph_domain,l.metadata_codec,l.expected_size,p.size,
         n.state AS graph_state,n.metadata_codec AS graph_codec,n.bytes AS graph_bytes,
         b.page_id IS NOT NULL AS payload_present,b.generation AS payload_generation,b.metadata_codec AS payload_codec,b.byte_size AS payload_size,
         EXISTS(SELECT 1 FROM mst2_metadata_gc_op o WHERE o.page_id=l.page_id AND o.generation=l.generation) AS tombstone,
         EXISTS(SELECT 1 FROM mst2_retention_node x WHERE x.node_id=l.node_id)
           OR EXISTS(SELECT 1 FROM mst2_retention_gc_op x WHERE x.node_id=l.node_id) AS generic_graph
         FROM jsonb_to_recordset($1::jsonb) p(id text,size integer)
         JOIN mst2_metadata_current c ON c.page_id=decode(p.id,'hex')
         JOIN mst2_metadata_lifetime l USING(page_id,generation)
         LEFT JOIN mst2_metadata_graph_node n USING(page_id,generation)
         LEFT JOIN mst2_metadata_payload b ON b.page_id=l.page_id ORDER BY l.page_id",
        [encoded.into()],
    )).await.map_err(internal)?;
    let mut bindings = BTreeMap::new();
    for row in rows {
        let state: String = row.try_get("", "state").map_err(internal)?;
        let generation: i64 = row.try_get("", "generation").map_err(internal)?;
        let size: i32 = row.try_get("", "size").map_err(internal)?;
        let graph: Option<String> = row.try_get("", "graph_state").map_err(internal)?;
        let payload: bool = row.try_get("", "payload_present").map_err(internal)?;
        if !["RESERVED", "LIVE"].contains(&state.as_str())
            || row.try_get::<bool>("", "tombstone").map_err(internal)?
            || row.try_get::<bool>("", "generic_graph").map_err(internal)?
            || graph.as_deref().is_some_and(|s| s != "LIVE")
        {
            return Err(unavailable(
                "qualified incarnation is removed, deleting, or has generic evidence",
            ));
        }
        if state == "LIVE" && (!payload || graph.is_none()) {
            return Err(unavailable(
                "LIVE qualified incarnation lost bytes or graph",
            ));
        }
        if row
            .try_get::<String>("", "graph_domain")
            .map_err(internal)?
            != "qualified-v1"
            || row.try_get::<i16>("", "metadata_codec").map_err(internal)?
                != plan.identity.metadata_codec as i16
            || row.try_get::<i32>("", "expected_size").map_err(internal)? != size
            || (graph.is_some()
                && (row
                    .try_get::<Option<i16>>("", "graph_codec")
                    .map_err(internal)?
                    != Some(plan.identity.metadata_codec as i16)
                    || row
                        .try_get::<Option<i64>>("", "graph_bytes")
                        .map_err(internal)?
                        != Some(i64::from(size))))
            || (payload
                && (row
                    .try_get::<Option<i64>>("", "payload_generation")
                    .map_err(internal)?
                    != Some(generation)
                    || row
                        .try_get::<Option<i16>>("", "payload_codec")
                        .map_err(internal)?
                        != Some(plan.identity.metadata_codec as i16)
                    || row
                        .try_get::<Option<i32>>("", "payload_size")
                        .map_err(internal)?
                        != Some(size)))
        {
            return Err(integrity(
                "qualified incarnation immutable profile mismatch",
            ));
        }
        let page: Vec<u8> = row.try_get("", "page_id").map_err(internal)?;
        bindings.insert(
            page.as_slice().try_into().map_err(internal)?,
            (generation, size as u64),
        );
    }
    if bindings.len() != plan.pages.len() {
        return Err(integrity("qualified lifetime allocation is incomplete"));
    }
    Ok(GenerationBindings(bindings))
}

pub(super) async fn retain_existing_roots(
    txn: &DatabaseTransaction,
    intent: &GenerationPrepareIntent,
) -> Result<(), SnapshotError> {
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_graph_root(prepare_id,storage_seal,page_id,generation)
         SELECT q.prepare_id,q.storage_seal,m.page_id,m.generation FROM mst2_metadata_prepare q
         JOIN mst2_metadata_prepare_page m USING(prepare_id)
         JOIN mst2_metadata_graph_node n USING(page_id,generation) WHERE q.prepare_id=$1 AND n.state='LIVE'
         ON CONFLICT(prepare_id,page_id,generation) DO NOTHING",
        [intent.prepare_id().into()],
    )).await.map_err(internal)?;
    Ok(())
}

pub(super) async fn check_lifetimes<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
) -> Result<(), SnapshotError> {
    if connection.query_one_raw(statement(
        "SELECT m.page_id FROM mst2_metadata_prepare_page m
         LEFT JOIN mst2_metadata_current c ON c.page_id=m.page_id AND c.generation=m.generation
         LEFT JOIN mst2_metadata_lifetime l ON l.page_id=m.page_id AND l.generation=m.generation
         LEFT JOIN mst2_metadata_graph_node n ON n.page_id=m.page_id AND n.generation=m.generation
         WHERE m.prepare_id=$1 AND (c.page_id IS NULL OR l.page_id IS NULL OR l.graph_domain<>'qualified-v1'
           OR l.state NOT IN ('RESERVED','LIVE') OR l.metadata_codec<>$2 OR l.expected_size<>m.expected_size
           OR ($3='COMMITTED' AND l.state<>'LIVE') OR (l.state='LIVE' AND n.page_id IS NULL)
           OR n.state<>'LIVE' OR n.metadata_codec<>$2 OR n.bytes<>m.expected_size
           OR EXISTS(SELECT 1 FROM mst2_metadata_gc_op o WHERE o.page_id=m.page_id AND o.generation=m.generation)
           OR EXISTS(SELECT 1 FROM mst2_retention_node x WHERE x.node_id=l.node_id)
           OR EXISTS(SELECT 1 FROM mst2_retention_gc_op x WHERE x.node_id=l.node_id)) LIMIT 1",
        [stored.record.prepare_id.clone().into(),stored.record.metadata_codec.into(),stored.record.state.clone().into()],
    )).await.map_err(internal)?.is_some() { return Err(unavailable("fixed qualified incarnation is no longer installable")); }
    Ok(())
}

pub(super) async fn finalize_graph(
    txn: &DatabaseTransaction,
    fixed: &FixedPlan,
    dag: &ValidatedMetadataDag,
) -> Result<PreparedMetadataReceipt, SnapshotError> {
    let actual_pages: BTreeSet<_> = dag.payloads().iter().map(|p| (p.id, p.size)).collect();
    let actual_edges: BTreeSet<_> = dag
        .edges()
        .iter()
        .map(|e| (e.parent.clone(), e.child.clone()))
        .collect();
    if dag.root() != fixed.stored.plan.root
        || actual_pages
            != fixed
                .stored
                .plan
                .pages
                .iter()
                .map(|(p, s)| (*p, *s))
                .collect()
        || actual_edges
            != fixed
                .stored
                .plan
                .edges
                .iter()
                .map(|(p, c)| (node_id(p), node_id(c)))
                .collect()
    {
        return Err(integrity(
            "qualified DAG observation differs from immutable plan",
        ));
    }
    if fixed.stored.record.state == "COMMITTED" {
        verify_graph(txn, fixed).await?;
        return fixed.stored.receipt();
    }
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_graph_node(page_id,generation,state,metadata_codec,bytes)
         SELECT m.page_id,m.generation,'LIVE',$2,m.expected_size FROM mst2_metadata_prepare_page m
         WHERE m.prepare_id=$1 AND NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_node n WHERE n.page_id=m.page_id AND n.generation=m.generation)",
        [fixed.intent.prepare_id().into(),fixed.stored.record.metadata_codec.into()],
    )).await.map_err(internal)?;
    let edges:Vec<_>=fixed.stored.plan.edges.iter().map(|(p,c)|json!({"parent":hex::encode(p),"pg":fixed.bindings.0[p].0,"child":hex::encode(c),"cg":fixed.bindings.0[c].0})).collect();
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_graph_edge(parent_page,parent_generation,child_page,child_generation)
         SELECT decode(e.parent,'hex'),e.pg,decode(e.child,'hex'),e.cg
         FROM jsonb_to_recordset($1::jsonb) e(parent text,pg bigint,child text,cg bigint)
         ON CONFLICT(parent_page,parent_generation,child_page,child_generation) DO NOTHING",
        [serde_json::to_string(&edges).map_err(internal)?.into()],
    )).await.map_err(internal)?;
    retain_existing_roots(txn, &fixed.intent).await?;
    verify_graph(txn, fixed).await?;
    let changed = txn
        .execute_raw(statement(
            "UPDATE mst2_metadata_prepare SET state='COMMITTED',committed_at=clock_timestamp()
         WHERE prepare_id=$1 AND state='PREPARING' AND storage_seal=$2",
            [
                fixed.intent.prepare_id().into(),
                fixed.intent.storage_seal.to_vec().into(),
            ],
        ))
        .await
        .map_err(internal)?;
    if changed.rows_affected() != 1 {
        return Err(integrity("qualified finalize lost its prepare CAS"));
    }
    fixed.stored.receipt()
}

pub(super) async fn verify_graph<C: ConnectionTrait>(
    connection: &C,
    fixed: &FixedPlan,
) -> Result<(), SnapshotError> {
    let rows=connection.query_all_raw(statement(
        "SELECT n.page_id,n.generation,n.state,n.metadata_codec,n.bytes,n.incoming_refs,
         (SELECT count(*) FROM mst2_metadata_graph_edge e WHERE e.child_page=n.page_id AND e.child_generation=n.generation) AS actual_refs
         FROM mst2_metadata_prepare_page m JOIN mst2_metadata_graph_node n USING(page_id,generation)
         WHERE m.prepare_id=$1 ORDER BY n.page_id LIMIT 4097",
        [fixed.intent.prepare_id().into()],
    )).await.map_err(internal)?;
    if rows.len() != fixed.bindings.0.len() {
        return Err(unavailable("qualified graph coverage is incomplete"));
    }
    for row in rows {
        let page: Vec<u8> = row.try_get("", "page_id").map_err(internal)?;
        let page: [u8; 32] = page.as_slice().try_into().map_err(internal)?;
        let &(generation, size) = fixed
            .bindings
            .0
            .get(&page)
            .ok_or_else(|| integrity("qualified graph has an extra node"))?;
        if row.try_get::<i64>("", "generation").map_err(internal)? != generation
            || row.try_get::<String>("", "state").map_err(internal)? != "LIVE"
            || row.try_get::<i16>("", "metadata_codec").map_err(internal)?
                != fixed.stored.record.metadata_codec
            || row.try_get::<i64>("", "bytes").map_err(internal)? != size as i64
            || row.try_get::<i64>("", "incoming_refs").map_err(internal)?
                != row.try_get::<i64>("", "actual_refs").map_err(internal)?
        {
            return Err(integrity("qualified graph profile or counter drift"));
        }
    }
    let rows=connection.query_all_raw(statement(
        "SELECT e.parent_page,e.parent_generation,e.child_page,e.child_generation FROM mst2_metadata_prepare_page m
         JOIN mst2_metadata_graph_edge e ON e.parent_page=m.page_id AND e.parent_generation=m.generation
         WHERE m.prepare_id=$1 LIMIT 16385",
        [fixed.intent.prepare_id().into()],
    )).await.map_err(internal)?;
    let mut actual = BTreeSet::new();
    for row in rows {
        let parent: Vec<u8> = row.try_get("", "parent_page").map_err(internal)?;
        let child: Vec<u8> = row.try_get("", "child_page").map_err(internal)?;
        actual.insert((
            parent,
            row.try_get::<i64>("", "parent_generation")
                .map_err(internal)?,
            child,
            row.try_get::<i64>("", "child_generation")
                .map_err(internal)?,
        ));
    }
    let expected: BTreeSet<_> = fixed
        .stored
        .plan
        .edges
        .iter()
        .map(|(p, c)| {
            (
                p.to_vec(),
                fixed.bindings.0[p].0,
                c.to_vec(),
                fixed.bindings.0[c].0,
            )
        })
        .collect();
    if actual != expected {
        return Err(integrity(
            "qualified graph edges differ from immutable plan",
        ));
    }
    verify_roots(connection, fixed, true).await
}

pub(super) async fn verify_roots<C: ConnectionTrait>(
    connection: &C,
    fixed: &FixedPlan,
    complete: bool,
) -> Result<(), SnapshotError> {
    let rows=connection.query_all_raw(statement(
        "SELECT page_id,generation,storage_seal FROM mst2_metadata_graph_root WHERE prepare_id=$1 LIMIT 4097",
        [fixed.intent.prepare_id().into()],
    )).await.map_err(internal)?;
    let mut actual = BTreeSet::new();
    for row in rows {
        let page: Vec<u8> = row.try_get("", "page_id").map_err(internal)?;
        let page: [u8; 32] = page.as_slice().try_into().map_err(internal)?;
        let generation: i64 = row.try_get("", "generation").map_err(internal)?;
        let seal: Vec<u8> = row.try_get("", "storage_seal").map_err(internal)?;
        if seal.as_slice() != fixed.intent.storage_seal
            || fixed.bindings.0.get(&page).map(|b| b.0) != Some(generation)
        {
            return Err(integrity(
                "qualified prepare root differs from its immutable seal",
            ));
        }
        actual.insert((page, generation));
    }
    if complete
        && actual
            != fixed
                .bindings
                .0
                .iter()
                .map(|(p, (g, _))| (*p, *g))
                .collect()
    {
        return Err(unavailable(
            "qualified prepare lost or transferred original roots",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "native_metadata_qualified_tests.rs"]
mod tests;
