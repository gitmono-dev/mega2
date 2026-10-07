//! Generation-bound preparation repository, isolated from HTTP lease authority.

use sha2::{Digest, Sha256};

use super::*;

const MAX_BINDINGS_BYTES: usize = 12 + 48 * 4096;
const MAX_PRIMARY_SCOPE_BYTES: usize = 16384;
const GENERIC_GRAPH_DOMAIN: &str = "generic-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphDomain {
    LegacyGeneric,
    Generic,
    Qualified,
}

impl GraphDomain {
    fn from_stored(domain: Option<&str>) -> Result<Self, SnapshotError> {
        match domain {
            None => Ok(Self::LegacyGeneric),
            Some(GENERIC_GRAPH_DOMAIN) => Ok(Self::Generic),
            Some("qualified-v1") => Ok(Self::Qualified),
            _ => Err(integrity("unknown stored metadata graph domain")),
        }
    }

    fn stored(self) -> Option<&'static str> {
        match self {
            Self::LegacyGeneric => None,
            Self::Generic => Some(GENERIC_GRAPH_DOMAIN),
            Self::Qualified => Some("qualified-v1"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationPrepareIntent {
    legacy: MetadataPrepareIntent,
    metadata_root: [u8; 32],
    root_generation: i64,
    bindings_digest: [u8; 32],
    primary_scope: Box<[u8]>,
    storage_seal: [u8; 32],
    graph_domain: GraphDomain,
}

impl GenerationPrepareIntent {
    pub fn prepare_id(&self) -> &str {
        self.legacy.prepare_id()
    }

    pub fn manifest_digest(&self) -> [u8; 32] {
        self.legacy.manifest_digest()
    }

    pub fn operation_id(&self) -> &str {
        self.legacy.operation_id()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationMetadataReceipt {
    legacy: PreparedMetadataReceipt,
    intent: GenerationPrepareIntent,
}

impl GenerationMetadataReceipt {
    pub fn intent(&self) -> &GenerationPrepareIntent {
        &self.intent
    }

    pub fn metadata_root(&self) -> [u8; 32] {
        self.intent.metadata_root
    }

    pub fn payload_bytes(&self) -> u64 {
        self.legacy.payload_bytes()
    }
}

#[derive(Debug)]
pub struct GenerationDagObservation {
    intent: GenerationPrepareIntent,
    dag: ValidatedMetadataDag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationPrepareObservation {
    Absent,
    Preparing(GenerationPrepareIntent),
    Committed(Box<GenerationMetadataReceipt>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GenerationBindings(BTreeMap<[u8; 32], (i64, u64)>);

impl GenerationBindings {
    fn encode(&self) -> Result<Vec<u8>, SnapshotError> {
        if self.0.is_empty() || self.0.len() > MetadataDagLimits::default().nodes {
            return Err(integrity("invalid metadata generation binding count"));
        }
        let mut bytes = Vec::with_capacity(12 + 48 * self.0.len());
        bytes.extend_from_slice(b"MST2GEN1");
        bytes.extend_from_slice(&(self.0.len() as u32).to_be_bytes());
        for (page, (generation, size)) in &self.0 {
            if *generation <= 0 || !(HEADER_LEN as u64..=PAGE_MAX_BYTES as u64).contains(size) {
                return Err(integrity("invalid metadata lifetime generation or size"));
            }
            bytes.extend_from_slice(page);
            bytes.extend_from_slice(&generation.to_be_bytes());
            bytes.extend_from_slice(&size.to_be_bytes());
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8], plan: &MetadataInstallPlan) -> Result<Self, SnapshotError> {
        if bytes.len() < 60 || bytes.len() > MAX_BINDINGS_BYTES || &bytes[..8] != b"MST2GEN1" {
            return Err(integrity("invalid metadata generation binding encoding"));
        }
        let count = u32::from_be_bytes(bytes[8..12].try_into().map_err(internal)?) as usize;
        if count != plan.pages.len() || bytes.len() != 12 + count * 48 {
            return Err(integrity(
                "metadata generation binding coverage differs from plan",
            ));
        }
        let mut bindings = BTreeMap::new();
        for entry in bytes[12..].as_chunks::<48>().0 {
            let page: [u8; 32] = entry[..32].try_into().map_err(internal)?;
            let generation = i64::from_be_bytes(entry[32..40].try_into().map_err(internal)?);
            let size = u64::from_be_bytes(entry[40..48].try_into().map_err(internal)?);
            if generation <= 0
                || plan.pages.get(&page) != Some(&size)
                || bindings.insert(page, (generation, size)).is_some()
            {
                return Err(integrity("invalid fixed metadata lifetime binding"));
            }
        }
        let result = Self(bindings);
        if result.encode()? != bytes {
            return Err(integrity("noncanonical metadata generation bindings"));
        }
        Ok(result)
    }
}

struct FixedPlan {
    stored: StoredPlan,
    bindings: GenerationBindings,
    intent: GenerationPrepareIntent,
}

#[derive(Clone)]
pub struct PostgresMetadataGenerationRepository {
    inner: PostgresMetadataInstallRepository,
    graph_domain: &'static str,
}

impl PostgresMetadataGenerationRepository {
    pub async fn new(connection: DatabaseConnection) -> Result<Self, SnapshotError> {
        Ok(Self {
            inner: PostgresMetadataInstallRepository::new(connection).await?,
            graph_domain: GENERIC_GRAPH_DOMAIN,
        })
    }

    pub async fn begin_intent(
        &self,
        operation_id: &str,
        prepared: &PreparedNativeMetadataRetention,
    ) -> Result<GenerationPrepareIntent, MetadataInstallError> {
        validate_operation_id(operation_id)?;
        let plan = prepared.install_plan()?;
        let digest = plan.digest()?;
        let primary_scope = self.primary_scope()?;
        let txn = self.inner.transaction().await?;
        let result = async {
            self.inner.barrier(&txn).await?;
            if let Some(fixed) = self.load_fixed_plan(&txn, operation_id, &digest).await? {
                return Ok(fixed.intent);
            }
            let bindings = allocate_lifetimes(&txn, &plan, self.graph_domain).await?;
            let canonical_bindings = bindings.encode()?;
            let bindings_digest = Sha256::digest(&canonical_bindings).into();
            let prepare_id = uuid::Uuid::new_v4().to_string();
            let intent = GenerationPrepareIntent {
                legacy: MetadataPrepareIntent { prepare_id: prepare_id.clone(), operation_id: operation_id.into(), manifest_digest: digest },
                metadata_root: plan.root,
                root_generation: bindings.0.get(&plan.root).ok_or_else(|| integrity("root lifetime is missing"))?.0,
                bindings_digest,
                storage_seal: seal(&prepare_id, &digest, &plan.root, &bindings_digest, &primary_scope,Some(self.graph_domain))?,
                primary_scope:primary_scope.into_boxed_slice(),
                graph_domain:GraphDomain::from_stored(Some(self.graph_domain))?,
            };
            let identity = &plan.identity;
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_prepare(prepare_id,operation_id,manifest_digest,canonical_plan,
                 source_domain,tagged_root_tree_oid,scope,schema_version,metadata_codec,materialization_policy,
                 fs_semantics,access_projection,verification_revision,projection_revision,metadata_root,
                 node_count,edge_count,total_bytes,state,canonical_bindings,bindings_digest,primary_scope,storage_seal,graph_domain)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,'PREPARING',$19,$20,$21,$22,$23)",
                [prepare_id.clone().into(),operation_id.into(),digest.to_vec().into(),plan.encode()?.into(),
                 identity.source_domain.clone().into(),identity.tagged_root_tree_oid.clone().into(),identity.scope.clone().into(),
                 (identity.schema_version as i16).into(),(identity.metadata_codec as i16).into(),
                 (identity.materialization_policy as i16).into(),(identity.fs_semantics as i16).into(),
                 (identity.access_projection as i16).into(),identity.verification_revision.into(),
                 (identity.projection_revision as i16).into(),plan.root.to_vec().into(),(plan.pages.len() as i32).into(),
                 (plan.edges.len() as i32).into(),(plan.total_bytes as i64).into(),canonical_bindings.into(),
                 intent.bindings_digest.to_vec().into(),intent.primary_scope.to_vec().into(),intent.storage_seal.to_vec().into(),self.graph_domain.into()],
            )).await.map_err(internal)?;
            let pages: Vec<_> = bindings.0.iter().map(|(page,(generation,size))|
                json!({"page_id":hex::encode(page),"generation":generation,"size":size})).collect();
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_prepare_page(prepare_id,page_id,generation,expected_size)
                 SELECT $1,decode(p.page_id,'hex'),p.generation,p.size
                 FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,generation bigint,size integer)",
                [prepare_id.clone().into(),serde_json::to_string(&pages).map_err(internal)?.into()],
            )).await.map_err(internal)?;
            // Reuse only already LIVE graph rows. RESERVED pages without a graph
            // are protected by the durable mappings, without inventing a DAG.
            txn.execute_raw(statement(
                "INSERT INTO mst2_retention_root(node_id,root_key,root_kind)
                 SELECT l.node_id,'prepare:'||p.prepare_id,'prepare'
                 FROM mst2_metadata_prepare_page p JOIN mst2_metadata_lifetime l
                   ON l.page_id=p.page_id AND l.generation=p.generation
                 JOIN mst2_retention_node n ON n.node_id=l.node_id AND n.state='LIVE'
                 WHERE p.prepare_id=$1 ON CONFLICT(node_id,root_key) DO NOTHING",
                [prepare_id.into()],
            )).await.map_err(internal)?;
            Ok(intent)
        }.await;
        commit(
            txn,
            result,
            operation_id,
            digest,
            MetadataCommitPhase::Intent,
        )
        .await
    }

    pub async fn install_page(
        &self,
        intent: &GenerationPrepareIntent,
        payload: &MetadataPagePayload,
    ) -> Result<(), MetadataInstallError> {
        self.install_pages(intent, std::slice::from_ref(payload))
            .await
    }

    pub async fn install_pages(
        &self,
        intent: &GenerationPrepareIntent,
        payloads: &[MetadataPagePayload],
    ) -> Result<(), MetadataInstallError> {
        if payloads.is_empty() || payloads.len() > 64 {
            return Err(SnapshotError::new(
                SnapshotErrorCode::LimitExceeded,
                "metadata installation batch must contain 1..=64 pages",
            )
            .into());
        }
        let mut ids = BTreeSet::new();
        for payload in payloads {
            validate_payload(payload)?;
            if !ids.insert(payload.id) {
                return Err(integrity("duplicate page in metadata installation batch").into());
            }
        }
        let txn = self.inner.transaction().await?;
        let result = async {
            self.inner.barrier(&txn).await?;
            let fixed = self.require_fixed_plan(&txn, intent).await?;
            if fixed.stored.record.state == "COMMITTED" {
                check_generation_payloads(&txn, &fixed).await?;
            }
            let mut pages = Vec::with_capacity(payloads.len());
            for page in payloads {
                let &(generation,size) = fixed.bindings.0.get(&page.id)
                    .ok_or_else(|| integrity("metadata payload is not a member of its fixed generation installation"))?;
                if size != page.size {
                    return Err(integrity("metadata payload size differs from its fixed lifetime"));
                }
                pages.push(json!({"page_id":hex::encode(page.id),"generation":generation,"size":size,"payload":hex::encode(&page.bytes)}));
            }
            let encoded = serde_json::to_string(&pages).map_err(internal)?;
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_payload(page_id,generation,metadata_codec,byte_size,payload)
                 SELECT decode(p.page_id,'hex'),p.generation,$1,p.size,decode(p.payload,'hex')
                 FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,generation bigint,size integer,payload text)
                 ON CONFLICT(page_id) DO NOTHING",
                [fixed.stored.record.metadata_codec.into(),encoded.clone().into()],
            )).await.map_err(internal)?;
            let bad=txn.query_one_raw(statement(
                "SELECT p.page_id FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,generation bigint,size integer,payload text)
                 LEFT JOIN mst2_metadata_payload b ON b.page_id=decode(p.page_id,'hex')
                 WHERE b.page_id IS NULL OR b.generation IS DISTINCT FROM p.generation OR b.metadata_codec<>$1
                   OR b.byte_size<>p.size OR b.payload<>decode(p.payload,'hex') LIMIT 1",
                [fixed.stored.record.metadata_codec.into(),encoded.into()],
            )).await.map_err(internal)?;
            if bad.is_some() {
                return Err(integrity("immutable payload conflicts with its exact metadata lifetime"));
            }
            Ok(())
        }.await;
        commit(
            txn,
            result,
            &intent.legacy.operation_id,
            intent.manifest_digest(),
            MetadataCommitPhase::Payload,
        )
        .await
    }

    /// Read, hash and parse the actual fixed-generation bytes outside the lock.
    pub async fn observe_installed_dag(
        &self,
        intent: &GenerationPrepareIntent,
    ) -> Result<GenerationDagObservation, SnapshotError> {
        self.inner
            .verify_primary_connection(&self.inner.connection)
            .await?;
        let fixed = self
            .require_fixed_plan(&self.inner.connection, intent)
            .await?;
        let dag = load_fixed_dag(&self.inner.connection, &fixed).await?;
        Ok(GenerationDagObservation {
            intent: intent.clone(),
            dag,
        })
    }

    pub async fn finalize(
        &self,
        intent: &GenerationPrepareIntent,
    ) -> Result<GenerationMetadataReceipt, MetadataInstallError> {
        let observation = self.observe_installed_dag(intent).await?;
        self.finalize_observation(intent, &observation).await
    }

    pub async fn finalize_observation(
        &self,
        intent: &GenerationPrepareIntent,
        observation: &GenerationDagObservation,
    ) -> Result<GenerationMetadataReceipt, MetadataInstallError> {
        if &observation.intent != intent {
            return Err(
                integrity("metadata observation belongs to another fixed installation").into(),
            );
        }
        let txn = self.inner.transaction().await?;
        let result = self.finalize_in_txn(&txn, intent, observation).await;
        commit(
            txn,
            result,
            &intent.legacy.operation_id,
            intent.manifest_digest(),
            MetadataCommitPhase::Finalize,
        )
        .await
    }

    // Only a successful outer commit or the recovery barrier exposes a receipt.
    async fn finalize_in_txn(
        &self,
        txn: &DatabaseTransaction,
        intent: &GenerationPrepareIntent,
        observation: &GenerationDagObservation,
    ) -> Result<GenerationMetadataReceipt, SnapshotError> {
        if &observation.intent != intent {
            return Err(integrity(
                "metadata observation belongs to another fixed installation",
            ));
        }
        self.inner.barrier(txn).await?;
        // Mapping INSERT guards take this same row lock. No late insertion
        // may cross coverage validation and the COMMITTED transition.
        lock_prepare(txn, intent.prepare_id()).await?;
        let fixed = self.require_fixed_plan(txn, intent).await?;
        check_generation_payloads(txn, &fixed).await?;
        let legacy = self
            .inner
            .finalize_stored_plan_in_txn(txn, &intent.legacy, &observation.dag, fixed.stored)
            .await?;
        txn.execute_raw(statement(
            "UPDATE mst2_metadata_lifetime l SET state='LIVE'
             FROM mst2_metadata_prepare_page p WHERE p.prepare_id=$1 AND p.page_id=l.page_id
               AND p.generation=l.generation AND l.state='RESERVED'",
            [intent.prepare_id().into()],
        ))
        .await
        .map_err(internal)?;
        Ok(GenerationMetadataReceipt {
            legacy,
            intent: intent.clone(),
        })
    }

    /// Compact proof for a future atomic handoff. No DAG/page scan under the
    /// mono writer lock: hash <=196620 binding bytes and check the root lifetime.
    pub(crate) async fn verify_receipt_in_txn(
        &self,
        txn: &DatabaseTransaction,
        receipt: &GenerationMetadataReceipt,
        tagged_root_tree_oid: &str,
        scope: &str,
    ) -> Result<(), SnapshotError> {
        self.require_scope(&receipt.intent)?;
        self.inner
            .verify_receipt_in_txn(txn, &receipt.legacy, tagged_root_tree_oid, scope)
            .await?;
        let row=txn.query_one_raw(statement(
            "SELECT p.bindings_digest,p.primary_scope,p.storage_seal,p.graph_domain,p.coverage_retired_at IS NOT NULL AS retired,
             CASE WHEN octet_length(p.canonical_bindings)<=$2 THEN sha256(p.canonical_bindings) END AS actual_bindings_digest,
             l.generation,l.state,l.metadata_codec,l.expected_size,m.generation AS root_generation,
             b.generation AS payload_generation,b.byte_size
             FROM mst2_metadata_prepare p LEFT JOIN mst2_metadata_prepare_page m
               ON m.prepare_id=p.prepare_id AND m.page_id=p.metadata_root
             LEFT JOIN mst2_metadata_lifetime l ON l.page_id=m.page_id AND l.generation=m.generation
             LEFT JOIN mst2_metadata_current c ON c.page_id=l.page_id AND c.generation=l.generation
             LEFT JOIN mst2_metadata_payload b ON b.page_id=m.page_id
             WHERE p.prepare_id=$1 AND c.page_id IS NOT NULL",
            [receipt.intent.prepare_id().into(),(MAX_BINDINGS_BYTES as i32).into()],
        )).await.map_err(internal)?.ok_or_else(|| unavailable("metadata generation receipt is missing"))?;
        let domain = row
            .try_get::<Option<String>>("", "graph_domain")
            .map_err(internal)?;
        if GraphDomain::from_stored(domain.as_deref())? != receipt.intent.graph_domain {
            return Err(integrity(
                "metadata generation receipt graph domain differs from storage",
            ));
        }
        if row.try_get::<bool>("", "retired").map_err(internal)? {
            return Err(unavailable(
                "metadata generation receipt coverage was retired",
            ));
        }
        for (column, expected) in [
            ("bindings_digest", receipt.intent.bindings_digest.as_slice()),
            (
                "actual_bindings_digest",
                receipt.intent.bindings_digest.as_slice(),
            ),
            ("primary_scope", receipt.intent.primary_scope.as_ref()),
            ("storage_seal", receipt.intent.storage_seal.as_slice()),
        ] {
            if row
                .try_get::<Option<Vec<u8>>>("", column)
                .map_err(internal)?
                .as_deref()
                != Some(expected)
            {
                return Err(integrity(
                    "metadata generation receipt seal differs from durable storage",
                ));
            }
        }
        let generation = row
            .try_get::<Option<i64>>("", "generation")
            .map_err(internal)?;
        if generation != Some(receipt.intent.root_generation)
            || row
                .try_get::<Option<i64>>("", "root_generation")
                .map_err(internal)?
                != generation
            || row
                .try_get::<Option<i64>>("", "payload_generation")
                .map_err(internal)?
                != generation
            || row
                .try_get::<Option<String>>("", "state")
                .map_err(internal)?
                .as_deref()
                != Some("LIVE")
            || row
                .try_get::<Option<i16>>("", "metadata_codec")
                .map_err(internal)?
                != Some(receipt.legacy.identity.metadata_codec as i16)
            || row
                .try_get::<Option<i32>>("", "expected_size")
                .map_err(internal)?
                != Some(receipt.legacy.root_payload_bytes as i32)
            || row
                .try_get::<Option<i32>>("", "byte_size")
                .map_err(internal)?
                != Some(receipt.legacy.root_payload_bytes as i32)
        {
            return Err(unavailable(
                "metadata generation receipt root lifetime is unavailable",
            ));
        }
        Ok(())
    }

    pub async fn inspect_prepare(
        &self,
        fresh_primary: &DatabaseConnection,
        operation_id: &str,
        digest: [u8; 32],
        phase: MetadataCommitPhase,
    ) -> Result<GenerationPrepareObservation, MetadataInstallError> {
        validate_operation_id(operation_id)?;
        let txn = fresh_primary
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(|_| uncertain(operation_id, digest, phase))?;
        if self.inner.barrier(&txn).await.is_err() {
            let _ = txn.rollback().await;
            return Err(uncertain(operation_id, digest, phase));
        }
        let result = async {
            match self.load_fixed_plan(&txn, operation_id, &digest).await? {
                None => Ok(GenerationPrepareObservation::Absent),
                Some(fixed) if fixed.stored.record.state == "COMMITTED" => {
                    check_generation_payloads(&txn, &fixed).await?;
                    load_fixed_dag(&txn, &fixed).await?;
                    verify_graph(&txn, &fixed.stored).await?;
                    Ok(GenerationPrepareObservation::Committed(Box::new(
                        GenerationMetadataReceipt {
                            legacy: fixed.stored.receipt()?,
                            intent: fixed.intent,
                        },
                    )))
                }
                Some(fixed) => Ok(GenerationPrepareObservation::Preparing(fixed.intent)),
            }
        }
        .await;
        txn.rollback()
            .await
            .map_err(|_| uncertain(operation_id, digest, phase))?;
        result.map_err(|error: SnapshotError| {
            if error.code == SnapshotErrorCode::Internal {
                uncertain(operation_id, digest, phase)
            } else {
                MetadataInstallError::Rejected(error)
            }
        })
    }

    async fn require_fixed_plan<C: ConnectionTrait>(
        &self,
        connection: &C,
        intent: &GenerationPrepareIntent,
    ) -> Result<FixedPlan, SnapshotError> {
        self.require_scope(intent)?;
        let fixed = self
            .load_fixed_plan(
                connection,
                &intent.legacy.operation_id,
                &intent.manifest_digest(),
            )
            .await?
            .ok_or_else(|| unavailable("fixed metadata preparation is missing"))?;
        if &fixed.intent != intent {
            return Err(integrity(
                "metadata intent differs from its fixed generation seal",
            ));
        }
        Ok(fixed)
    }

    async fn load_fixed_plan<C: ConnectionTrait>(
        &self,
        connection: &C,
        operation_id: &str,
        digest: &[u8; 32],
    ) -> Result<Option<FixedPlan>, SnapshotError> {
        let Some(stored) = load_plan(connection, operation_id, digest).await? else {
            return Ok(None);
        };
        if stored.record.coverage_retired_at.is_some() {
            return Err(unavailable("metadata preparation coverage was retired"));
        }
        let canonical = stored
            .record
            .canonical_bindings
            .as_deref()
            .ok_or_else(|| unavailable("legacy preparation has no generation seal"))?;
        let bindings = GenerationBindings::decode(canonical, &stored.plan)?;
        let bindings_digest: [u8; 32] = Sha256::digest(canonical).into();
        let primary_scope = stored
            .record
            .primary_scope
            .clone()
            .ok_or_else(|| integrity("fixed preparation primary scope is missing"))?;
        let storage_seal = seal(
            &stored.record.prepare_id,
            digest,
            &stored.plan.root,
            &bindings_digest,
            &primary_scope,
            stored.record.graph_domain.as_deref(),
        )?;
        if stored.record.bindings_digest.as_deref() != Some(bindings_digest.as_slice())
            || stored.record.storage_seal.as_deref() != Some(storage_seal.as_slice())
        {
            return Err(integrity("metadata generation seal is corrupt"));
        }
        let intent = GenerationPrepareIntent {
            legacy: stored.intent()?,
            metadata_root: stored.plan.root,
            root_generation: bindings
                .0
                .get(&stored.plan.root)
                .ok_or_else(|| integrity("root lifetime is missing"))?
                .0,
            bindings_digest,
            primary_scope: primary_scope.into_boxed_slice(),
            storage_seal,
            graph_domain: GraphDomain::from_stored(stored.record.graph_domain.as_deref())?,
        };
        self.require_scope(&intent)?;
        let mut actual = BTreeMap::new();
        for row in &stored.prepare_pages {
            let page: [u8; 32] = row.page_id.as_slice().try_into().map_err(internal)?;
            let generation = row
                .generation
                .ok_or_else(|| integrity("fixed metadata page has no generation"))?;
            actual.insert(page, (generation, row.expected_size as u64));
        }
        if actual != bindings.0 {
            return Err(integrity(
                "metadata mappings differ from complete fixed generation bindings",
            ));
        }
        check_lifetimes(connection, &stored).await?;
        Ok(Some(FixedPlan {
            stored,
            bindings,
            intent,
        }))
    }

    fn primary_scope(&self) -> Result<Vec<u8>, SnapshotError> {
        let scope = &self.inner.storage_scope;
        let bytes = serde_json::to_vec(&(
            &scope.storage_uuid,
            &scope.database,
            scope.database_oid,
            &scope.schema,
            scope.schema_oid,
            &scope.server_address,
            scope.server_port,
        ))
        .map_err(internal)?;
        if bytes.is_empty() || bytes.len() > MAX_PRIMARY_SCOPE_BYTES {
            return Err(integrity("invalid captured metadata primary scope size"));
        }
        Ok(bytes)
    }

    fn require_scope(&self, intent: &GenerationPrepareIntent) -> Result<(), SnapshotError> {
        if intent.primary_scope.as_ref() != self.primary_scope()?.as_slice() {
            return Err(integrity(
                "metadata seal belongs to another captured primary storage scope",
            ));
        }
        if intent.graph_domain.stored().unwrap_or(GENERIC_GRAPH_DOMAIN) != self.graph_domain {
            return Err(integrity(
                "metadata seal belongs to another immutable graph domain",
            ));
        }
        Ok(())
    }
}

fn seal(
    prepare_id: &str,
    manifest: &[u8; 32],
    root: &[u8; 32],
    bindings: &[u8; 32],
    scope: &[u8],
    graph_domain: Option<&str>,
) -> Result<[u8; 32], SnapshotError> {
    if scope.is_empty() || scope.len() > MAX_PRIMARY_SCOPE_BYTES {
        return Err(integrity("invalid metadata generation seal primary scope"));
    }
    let id = uuid::Uuid::parse_str(prepare_id).map_err(internal)?;
    let mut hash = Sha256::new();
    match graph_domain {
        None => hash.update(b"MST2-METADATA-STORAGE-SEAL-1\0"),
        Some(domain) => {
            if ![GENERIC_GRAPH_DOMAIN, "qualified-v1"].contains(&domain) {
                return Err(integrity("unknown metadata graph domain"));
            }
            hash.update(b"MST2-METADATA-STORAGE-SEAL-2\0");
            hash.update((domain.len() as u32).to_be_bytes());
            hash.update(domain.as_bytes());
        }
    }
    hash.update(id.as_bytes());
    hash.update(manifest);
    hash.update(root);
    hash.update(bindings);
    hash.update((scope.len() as u32).to_be_bytes());
    hash.update(scope);
    Ok(hash.finalize().into())
}

async fn allocate_lifetimes(
    txn: &DatabaseTransaction,
    plan: &MetadataInstallPlan,
    graph_domain: &str,
) -> Result<GenerationBindings, SnapshotError> {
    let pages: Vec<_> = plan
        .pages
        .iter()
        .map(|(page, size)| json!({"page_id":hex::encode(page),"size":size}))
        .collect();
    let encoded = serde_json::to_string(&pages).map_err(internal)?;
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size)
         SELECT decode(p.page_id,'hex'),'page:sha256:'||p.page_id,1,'RESERVED',$1,p.size
         FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,size integer)
         ON CONFLICT(page_id,generation) DO NOTHING",
        [(plan.identity.metadata_codec as i16).into(),encoded.clone().into()],
    )).await.map_err(internal)?;
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_current(page_id,generation)
         SELECT decode(p.page_id,'hex'),1 FROM jsonb_to_recordset($1::jsonb) AS p(page_id text,size integer)
         ON CONFLICT(page_id) DO NOTHING",
        [encoded.clone().into()],
    )).await.map_err(internal)?;
    if txn.query_one_raw(statement(
        "SELECT p.page_id FROM jsonb_to_recordset($1::jsonb) AS p(page_id text,size integer)
         JOIN mst2_metadata_prepare_page m ON m.page_id=decode(p.page_id,'hex')
         JOIN mst2_metadata_current c ON c.page_id=m.page_id AND c.generation=m.generation
         JOIN mst2_metadata_prepare q ON q.prepare_id=m.prepare_id
         WHERE coalesce(q.graph_domain,'generic-v1')<>$2
           AND (q.state='PREPARING' OR (q.state='COMMITTED' AND q.coverage_retired_at IS NULL)) LIMIT 1",
        [encoded.clone().into(),graph_domain.into()],
    )).await.map_err(internal)?.is_some() {
        return Err(unavailable("metadata lifetime is covered by another graph domain"));
    }
    let rows=txn.query_all_raw(statement(
        "SELECT decode(p.page_id,'hex') AS page_id,p.size,l.generation,l.state,l.metadata_codec,l.expected_size,
         n.state AS graph_state,n.kind AS graph_kind,n.bytes AS graph_bytes,
         b.page_id IS NOT NULL AS payload_present,b.generation AS payload_generation,
         b.metadata_codec AS payload_codec,b.byte_size AS payload_size,
         EXISTS(SELECT 1 FROM mst2_retention_gc_op g WHERE g.node_id=l.node_id
           AND g.operation='REMOVE' AND g.state IN ('PENDING','APPLIED')) AS tombstone
         FROM jsonb_to_recordset($1::jsonb) AS p(page_id text,size integer)
         JOIN mst2_metadata_current c ON c.page_id=decode(p.page_id,'hex')
         JOIN mst2_metadata_lifetime l ON l.page_id=c.page_id AND l.generation=c.generation
         LEFT JOIN mst2_retention_node n ON n.node_id=l.node_id
         LEFT JOIN mst2_metadata_payload b ON b.page_id=l.page_id ORDER BY l.page_id",
        [encoded.into()],
    )).await.map_err(internal)?;
    let mut bindings = BTreeMap::new();
    for row in rows {
        let state: String = row.try_get("", "state").map_err(internal)?;
        let generation: i64 = row.try_get("", "generation").map_err(internal)?;
        let size: i32 = row.try_get("", "size").map_err(internal)?;
        if state == "LIVE"
            && (!row
                .try_get::<bool>("", "payload_present")
                .map_err(internal)?
                || row
                    .try_get::<Option<String>>("", "graph_state")
                    .map_err(internal)?
                    .is_none())
        {
            return Err(unavailable(
                "LIVE metadata lifetime has lost its payload or graph",
            ));
        }
        if !["RESERVED", "LIVE"].contains(&state.as_str())
            || row.try_get::<bool>("", "tombstone").map_err(internal)?
            || row
                .try_get::<Option<String>>("", "graph_state")
                .map_err(internal)?
                .is_some_and(|state| state != "LIVE")
        {
            return Err(unavailable("metadata lifetime is deleting or removed"));
        }
        if generation <= 0
            || row.try_get::<i16>("", "metadata_codec").map_err(internal)?
                != plan.identity.metadata_codec as i16
            || row.try_get::<i32>("", "expected_size").map_err(internal)? != size
            || row
                .try_get::<Option<String>>("", "graph_kind")
                .map_err(internal)?
                .is_some_and(|kind| kind != "page")
            || row
                .try_get::<Option<i64>>("", "graph_bytes")
                .map_err(internal)?
                .is_some_and(|bytes| bytes != size as i64)
        {
            return Err(integrity(
                "metadata lifetime profile conflicts with fixed plan",
            ));
        }
        if row
            .try_get::<bool>("", "payload_present")
            .map_err(internal)?
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
                    != Some(size))
        {
            return Err(integrity(
                "existing metadata payload has another or unbound lifetime",
            ));
        }
        let page: Vec<u8> = row.try_get("", "page_id").map_err(internal)?;
        bindings.insert(
            page.as_slice().try_into().map_err(internal)?,
            (generation, size as u64),
        );
    }
    if bindings.len() != plan.pages.len() {
        return Err(integrity("incomplete metadata lifetime allocation"));
    }
    Ok(GenerationBindings(bindings))
}

async fn check_lifetimes<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
) -> Result<(), SnapshotError> {
    let row=connection.query_one_raw(statement(
        "SELECT p.page_id,l.generation,l.state,l.metadata_codec,l.expected_size,p.generation AS expected_generation,
         n.state AS graph_state,n.kind AS graph_kind,n.bytes AS graph_bytes,
         EXISTS(SELECT 1 FROM mst2_retention_gc_op g WHERE g.node_id=l.node_id
           AND g.operation='REMOVE' AND g.state IN ('PENDING','APPLIED')) AS tombstone
         FROM mst2_metadata_prepare_page p LEFT JOIN mst2_metadata_lifetime l ON l.page_id=p.page_id AND l.generation=p.generation
         LEFT JOIN mst2_metadata_current c ON c.page_id=p.page_id AND c.generation=p.generation
         LEFT JOIN mst2_retention_node n ON n.node_id=l.node_id WHERE p.prepare_id=$1
           AND (l.page_id IS NULL OR c.page_id IS NULL OR l.generation IS DISTINCT FROM p.generation OR l.metadata_codec<>$2
             OR l.expected_size<>p.expected_size OR l.state NOT IN ('RESERVED','LIVE')
             OR ($3='COMMITTED' AND l.state<>'LIVE')
             OR (l.state='LIVE' AND n.node_id IS NULL)
             OR n.state<>'LIVE' OR n.kind<>'page' OR n.bytes<>p.expected_size
             OR EXISTS(SELECT 1 FROM mst2_retention_gc_op g WHERE g.node_id=l.node_id
               AND g.operation='REMOVE' AND g.state IN ('PENDING','APPLIED'))) LIMIT 1",
        [stored.record.prepare_id.clone().into(),stored.record.metadata_codec.into(),stored.record.state.clone().into()],
    )).await.map_err(internal)?;
    if let Some(row) = row {
        if row
            .try_get::<Option<String>>("", "state")
            .map_err(internal)?
            .is_some_and(|state| !["RESERVED", "LIVE"].contains(&state.as_str()))
            || row
                .try_get::<Option<String>>("", "graph_state")
                .map_err(internal)?
                .is_some_and(|state| state != "LIVE")
            || row.try_get::<bool>("", "tombstone").map_err(internal)?
        {
            return Err(unavailable(
                "fixed metadata lifetime is no longer installable",
            ));
        }
        return Err(integrity(
            "fixed metadata lifetime registry conflicts with its bindings",
        ));
    }
    Ok(())
}

async fn check_generation_payloads<C: ConnectionTrait>(
    connection: &C,
    fixed: &FixedPlan,
) -> Result<(), SnapshotError> {
    let row=connection.query_one_raw(statement(
        "SELECT p.page_id FROM mst2_metadata_prepare_page p LEFT JOIN mst2_metadata_payload b ON b.page_id=p.page_id
         WHERE p.prepare_id=$1 AND (b.page_id IS NULL OR b.generation IS DISTINCT FROM p.generation
           OR b.metadata_codec<>$2 OR b.byte_size<>p.expected_size OR octet_length(b.payload)<>p.expected_size) LIMIT 1",
        [fixed.intent.prepare_id().into(),fixed.stored.record.metadata_codec.into()],
    )).await.map_err(internal)?;
    if row.is_some() {
        return Err(unavailable(
            "exact metadata generation payload coverage is incomplete",
        ));
    }
    Ok(())
}

async fn load_fixed_dag<C: ConnectionTrait>(
    connection: &C,
    fixed: &FixedPlan,
) -> Result<ValidatedMetadataDag, SnapshotError> {
    let rows = connection
        .query_all_raw(statement(
            "SELECT b.page_id,b.generation,b.metadata_codec,b.byte_size,b.payload
         FROM mst2_metadata_prepare_page p JOIN mst2_metadata_payload b
           ON b.page_id=p.page_id AND b.generation=p.generation WHERE p.prepare_id=$1",
            [fixed.intent.prepare_id().into()],
        ))
        .await
        .map_err(internal)?;
    let mut pages = Vec::with_capacity(rows.len());
    for row in rows {
        let id: Vec<u8> = row.try_get("", "page_id").map_err(internal)?;
        let id: [u8; 32] = id.as_slice().try_into().map_err(internal)?;
        let generation: i64 = row.try_get("", "generation").map_err(internal)?;
        let size: i32 = row.try_get("", "byte_size").map_err(internal)?;
        if fixed.bindings.0.get(&id) != Some(&(generation, size as u64))
            || row.try_get::<i16>("", "metadata_codec").map_err(internal)?
                != fixed.stored.record.metadata_codec
        {
            return Err(integrity(
                "installed bytes differ from their fixed lifetime binding",
            ));
        }
        let payload = MetadataPagePayload {
            id,
            size: size as u64,
            bytes: row.try_get("", "payload").map_err(internal)?,
        };
        validate_payload(&payload)?;
        pages.push(payload);
    }
    if pages.len() != fixed.bindings.0.len() {
        return Err(unavailable(
            "exact metadata generation payload coverage is incomplete",
        ));
    }
    ValidatedMetadataDag::validate(
        MetadataDagCandidate {
            metadata_codec: fixed.stored.plan.identity.metadata_codec,
            root: fixed.stored.plan.root,
            pages,
            edges: fixed.stored.plan.edges.iter().copied().collect(),
        },
        MetadataDagLimits::default(),
    )
}

async fn lock_prepare(txn: &DatabaseTransaction, prepare_id: &str) -> Result<(), SnapshotError> {
    txn.query_one_raw(statement(
        "SELECT prepare_id FROM mst2_metadata_prepare WHERE prepare_id=$1 FOR UPDATE",
        [prepare_id.into()],
    ))
    .await
    .map_err(internal)?
    .ok_or_else(|| unavailable("fixed metadata prepare is missing"))?;
    Ok(())
}

#[cfg(test)]
#[path = "native_metadata_generation_tests.rs"]
mod tests;

#[path = "native_metadata_history.rs"]
pub mod history;
