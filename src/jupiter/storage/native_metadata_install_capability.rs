//! Reuse pure plan validation only after its durable membership is frozen.

use sha2::{Digest, Sha256};

use super::{
    BTreeMap, BTreeSet, ConnectionTrait, DatabaseTransaction, MetadataCommitPhase,
    MetadataInstallError, MetadataInstallIdentity, MetadataPagePayload, MetadataPrepareIntent,
    PostgresMetadataInstallRepository, PrimaryStorageScope, QueryResult, SnapshotError,
    SnapshotErrorCode, check_payload_coverage, commit, integrity, internal, json,
    load_installed_dag, require_plan, statement, unavailable, validate_payload, verify_graph,
};

const SEAL_DOMAIN: &[u8] = b"MST2-LEGACY-INSTALL-CAPABILITY-1\0";

#[derive(Debug)]
pub(crate) struct ValidatedLegacyInstallCapability {
    intent: MetadataPrepareIntent,
    identity: MetadataInstallIdentity,
    root: [u8; 32],
    edge_count: usize,
    total_bytes: u64,
    members: BTreeMap<[u8; 32], (i32, Option<i64>)>,
    members_digest: [u8; 32],
    scope: PrimaryStorageScope,
    primary_scope: Vec<u8>,
    install_seal: [u8; 32],
}

impl PostgresMetadataInstallRepository {
    pub(crate) async fn mint_legacy_install_capability(
        &self,
        intent: &MetadataPrepareIntent,
    ) -> Result<super::ValidatedLegacyInstallCapability, MetadataInstallError> {
        let txn = self.transaction().await?;
        let result = async {
            self.capability_barrier(&txn).await?;
            let stored = require_plan(&txn, intent).await?;
            let record = &stored.record;
            if record.storage_seal.is_some()
                || record.canonical_bindings.is_some()
                || record.bindings_digest.is_some()
                || record.primary_scope.is_some()
                || record.graph_domain.is_some()
                || record.coverage_retired_at.is_some()
                || record.aborted_at.is_some()
            {
                return Err(unavailable("install capability requires an active unbound legacy preparation"));
            }
            let mut members = BTreeMap::new();
            for member in &stored.prepare_pages {
                if member.generation.is_some() {
                    return Err(integrity("legacy install capability cannot adopt generation bindings"));
                }
                let id = member.page_id.as_slice().try_into().map_err(internal)?;
                members.insert(id, (member.expected_size, member.generation));
            }
            if record.state == "COMMITTED" {
                load_installed_dag(&txn, &stored).await?;
                check_payload_coverage(&txn, &stored).await?;
                verify_graph(&txn, &stored).await?;
            }
            let members_digest = member_digest(&members);
            let primary_scope = scope_bytes(&self.storage_scope)?;
            let install_seal = seal(intent, &members_digest, &primary_scope)?;
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_install_seal(prepare_id,operation_id,manifest_digest,members_digest,primary_scope,install_seal)
                 VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(prepare_id) DO NOTHING",
                [intent.prepare_id.clone().into(),intent.operation_id.clone().into(),
                 intent.manifest_digest.to_vec().into(),members_digest.to_vec().into(),
                 primary_scope.clone().into(),install_seal.to_vec().into()],
            )).await.map_err(internal)?;
            let capability = ValidatedLegacyInstallCapability {
                intent: intent.clone(), identity: stored.plan.identity, root: stored.plan.root,
                edge_count: stored.plan.edges.len(), total_bytes: stored.plan.total_bytes,
                members,members_digest,scope:self.storage_scope.clone(),primary_scope,install_seal,
            };
            read_registered_prepare(&txn, &capability).await?;
            Ok(capability)
        }.await;
        commit(
            txn,
            result,
            &intent.operation_id,
            intent.manifest_digest,
            MetadataCommitPhase::Intent,
        )
        .await
    }

    pub(crate) async fn install_pages_validated(
        &self,
        capability: &ValidatedLegacyInstallCapability,
        payloads: &[MetadataPagePayload],
    ) -> Result<(), MetadataInstallError> {
        if capability.scope != self.storage_scope {
            return Err(
                integrity("install capability belongs to another captured primary scope").into(),
            );
        }
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
            if capability
                .members
                .get(&payload.id)
                .map(|member| member.0 as u64)
                != Some(payload.size)
            {
                return Err(integrity(
                    "metadata payload is not a member of its validated installation",
                )
                .into());
            }
        }
        let pages: Vec<_> = payloads
            .iter()
            .map(|p| {
                json!({
                    "page_id":hex::encode(p.id),"size":p.size,"payload":hex::encode(&p.bytes)
                })
            })
            .collect();
        let encoded = serde_json::to_string(&pages).map_err(internal)?;
        let txn = self.transaction().await?;
        let result = async {
            self.capability_barrier(&txn).await?;
            let state = read_registered_prepare(&txn,capability).await?;
            check_requested_members(&txn,capability,&encoded,payloads.len()).await?;
            if state == "COMMITTED" {
                // Committed replay retains the complete receipt oracle and never repairs bytes.
                let stored = require_plan(&txn,&capability.intent).await?;
                load_installed_dag(&txn,&stored).await?;
                check_payload_coverage(&txn,&stored).await?;
                verify_graph(&txn,&stored).await?;
            } else {
                txn.execute_raw(statement(
                    "INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload)
                     SELECT decode(p.page_id,'hex'),$1,p.size,decode(p.payload,'hex')
                     FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,size integer,payload text)
                     ON CONFLICT(page_id) DO NOTHING",
                    [(capability.identity.metadata_codec as i16).into(),encoded.clone().into()],
                )).await.map_err(internal)?;
            }
            let bad = txn.query_one_raw(statement(
                "SELECT p.page_id FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,size integer,payload text)
                 LEFT JOIN mst2_metadata_payload b ON b.page_id=decode(p.page_id,'hex')
                 WHERE b.page_id IS NULL OR b.metadata_codec<>$1 OR b.byte_size<>p.size
                   OR b.payload<>decode(p.payload,'hex') LIMIT 1",
                [(capability.identity.metadata_codec as i16).into(),encoded.into()],
            )).await.map_err(internal)?;
            if bad.is_some() {
                return Err(integrity("immutable metadata payload identity conflicts with stored bytes"));
            }
            Ok(())
        }.await;
        commit(
            txn,
            result,
            &capability.intent.operation_id,
            capability.intent.manifest_digest,
            MetadataCommitPhase::Payload,
        )
        .await
    }

    pub(super) async fn capability_barrier(
        &self,
        txn: &DatabaseTransaction,
    ) -> Result<(), SnapshotError> {
        let schema = txn
            .query_one_raw(statement(
                "SELECT pg_catalog.current_schema() AS schema",
                [],
            ))
            .await
            .map_err(internal)?
            .ok_or_else(|| internal("primary schema is missing"))?
            .try_get::<String>("", "schema")
            .map_err(internal)?;
        if schema != self.storage_scope.schema {
            return Err(integrity(
                "install capability transaction is outside its captured schema",
            ));
        }
        txn.execute_raw(statement(
            "SELECT pg_catalog.set_config('search_path',pg_catalog.quote_ident($1)||',pg_catalog,pg_temp',true)",
            [schema.into()],
        )).await.map_err(internal)?;
        self.barrier(txn).await
    }
}

async fn read_registered_prepare(
    txn: &DatabaseTransaction,
    capability: &ValidatedLegacyInstallCapability,
) -> Result<String, SnapshotError> {
    let row=txn.query_one_raw(statement(
        "SELECT p.prepare_id,p.operation_id,p.manifest_digest,p.source_domain,p.tagged_root_tree_oid,p.scope,
         p.schema_version,p.metadata_codec,p.materialization_policy,p.fs_semantics,p.access_projection,
         p.verification_revision,p.projection_revision,p.metadata_root,p.node_count,p.edge_count,p.total_bytes,
         p.state,p.committed_at IS NOT NULL AS committed,p.aborted_at IS NOT NULL AS aborted,
         p.coverage_retired_at IS NOT NULL AS retired,
         p.storage_seal IS NOT NULL OR p.canonical_bindings IS NOT NULL OR p.bindings_digest IS NOT NULL
           OR p.primary_scope IS NOT NULL OR p.graph_domain IS NOT NULL AS generation_bound,
         s.operation_id AS sealed_operation,s.manifest_digest AS sealed_manifest,s.members_digest,
         s.primary_scope AS sealed_scope,s.install_seal
         FROM mst2_metadata_prepare p JOIN mst2_metadata_install_seal s USING(prepare_id)
         WHERE p.prepare_id=$1",
        [capability.intent.prepare_id.clone().into()],
    )).await.map_err(internal)?.ok_or_else(||unavailable("validated install registration is missing"))?;
    let identity = &capability.identity;
    for (column, expected) in [
        ("prepare_id", &capability.intent.prepare_id),
        ("operation_id", &capability.intent.operation_id),
        ("sealed_operation", &capability.intent.operation_id),
        ("source_domain", &identity.source_domain),
        ("tagged_root_tree_oid", &identity.tagged_root_tree_oid),
        ("scope", &identity.scope),
    ] {
        if row.try_get::<String>("", column).map_err(internal)? != *expected {
            return Err(integrity(
                "validated install identity differs from durable registration",
            ));
        }
    }
    for (column, expected) in [
        ("schema_version", identity.schema_version),
        ("metadata_codec", identity.metadata_codec),
        ("materialization_policy", identity.materialization_policy),
        ("fs_semantics", identity.fs_semantics),
        ("access_projection", identity.access_projection),
        ("projection_revision", identity.projection_revision),
    ] {
        if row.try_get::<i16>("", column).map_err(internal)? != expected as i16 {
            return Err(integrity(
                "validated install profile differs from durable registration",
            ));
        }
    }
    for (column, expected) in [
        (
            "manifest_digest",
            capability.intent.manifest_digest.as_slice(),
        ),
        (
            "sealed_manifest",
            capability.intent.manifest_digest.as_slice(),
        ),
        ("metadata_root", capability.root.as_slice()),
        ("members_digest", capability.members_digest.as_slice()),
        ("sealed_scope", capability.primary_scope.as_slice()),
        ("install_seal", capability.install_seal.as_slice()),
    ] {
        if row
            .try_get::<Vec<u8>>("", column)
            .map_err(internal)?
            .as_slice()
            != expected
        {
            return Err(integrity(
                "validated install proof differs from durable registration",
            ));
        }
    }
    let state = row.try_get::<String>("", "state").map_err(internal)?;
    if row
        .try_get::<i32>("", "verification_revision")
        .map_err(internal)?
        != identity.verification_revision
        || row.try_get::<i32>("", "node_count").map_err(internal)? as usize
            != capability.members.len()
        || row.try_get::<i32>("", "edge_count").map_err(internal)? as usize != capability.edge_count
        || row.try_get::<i64>("", "total_bytes").map_err(internal)? as u64 != capability.total_bytes
        || !["PREPARING", "COMMITTED"].contains(&state.as_str())
        || row.try_get::<bool>("", "committed").map_err(internal)? != (state == "COMMITTED")
        || row.try_get::<bool>("", "aborted").map_err(internal)?
        || row.try_get::<bool>("", "retired").map_err(internal)?
        || row
            .try_get::<bool>("", "generation_bound")
            .map_err(internal)?
    {
        return Err(unavailable(
            "validated install preparation is no longer active with its fixed profile",
        ));
    }
    Ok(state)
}

async fn check_requested_members(
    txn: &DatabaseTransaction,
    capability: &ValidatedLegacyInstallCapability,
    encoded: &str,
    expected_count: usize,
) -> Result<(), SnapshotError> {
    let rows=txn.query_all_raw(statement(
        "SELECT decode(p.page_id,'hex') AS page_id,m.expected_size,m.generation AS member_generation,
         b.page_id IS NOT NULL AS payload_present,b.generation AS payload_generation,
         c.generation AS current_generation,l.generation AS lifetime_generation,l.graph_domain,
         l.state AS lifetime_state,l.metadata_codec AS lifetime_codec,l.expected_size AS lifetime_size,
         n.state AS graph_state,n.kind AS graph_kind,n.bytes AS graph_bytes,
         EXISTS(SELECT 1 FROM mst2_retention_gc_op g WHERE g.node_id='page:sha256:'||p.page_id
           AND g.operation='REMOVE' AND g.state IN ('PENDING','APPLIED')) AS tombstone
         FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,size integer,payload text)
         LEFT JOIN mst2_metadata_prepare_page m ON m.prepare_id=$1 AND m.page_id=decode(p.page_id,'hex')
         LEFT JOIN mst2_metadata_payload b ON b.page_id=m.page_id
         LEFT JOIN mst2_metadata_current c ON c.page_id=m.page_id
         LEFT JOIN mst2_metadata_lifetime l ON l.page_id=c.page_id AND l.generation=c.generation
         LEFT JOIN mst2_retention_node n ON n.node_id='page:sha256:'||p.page_id",
        [capability.intent.prepare_id.clone().into(),encoded.into()],
    )).await.map_err(internal)?;
    if rows.len() != expected_count {
        return Err(integrity("requested install membership is incomplete"));
    }
    for row in rows {
        let page: Vec<u8> = row.try_get("", "page_id").map_err(internal)?;
        let page: [u8; 32] = page.as_slice().try_into().map_err(internal)?;
        let size = row
            .try_get::<Option<i32>>("", "expected_size")
            .map_err(internal)?;
        let generation = row
            .try_get::<Option<i64>>("", "member_generation")
            .map_err(internal)?;
        if capability.members.get(&page).copied() != size.map(|size| (size, generation)) {
            return Err(integrity(
                "requested member differs from validated installation",
            ));
        }
        check_physical_member(
            &row,
            capability.identity.metadata_codec as i16,
            size.ok_or_else(|| integrity("requested member is missing"))?,
        )?;
    }
    Ok(())
}

fn check_physical_member(row: &QueryResult, codec: i16, size: i32) -> Result<(), SnapshotError> {
    let current = row
        .try_get::<Option<i64>>("", "current_generation")
        .map_err(internal)?;
    let lifetime = row
        .try_get::<Option<i64>>("", "lifetime_generation")
        .map_err(internal)?;
    let payload = row
        .try_get::<Option<i64>>("", "payload_generation")
        .map_err(internal)?;
    let graph = row
        .try_get::<Option<String>>("", "graph_state")
        .map_err(internal)?;
    if row.try_get::<bool>("", "tombstone").map_err(internal)?
        || graph.as_deref().is_some_and(|state| state != "LIVE")
        || row
            .try_get::<Option<String>>("", "graph_kind")
            .map_err(internal)?
            .is_some_and(|kind| kind != "page")
        || row
            .try_get::<Option<i64>>("", "graph_bytes")
            .map_err(internal)?
            .is_some_and(|bytes| bytes != size as i64)
    {
        return Err(unavailable(
            "requested generic metadata graph is unavailable",
        ));
    }
    if payload.is_some() && (payload != current || payload != lifetime) {
        return Err(integrity(
            "generic payload is not its exact current physical lifetime",
        ));
    }
    if current.is_some() {
        let state = row
            .try_get::<Option<String>>("", "lifetime_state")
            .map_err(internal)?;
        if current != lifetime
            || current.is_some_and(|generation| generation <= 0)
            || row
                .try_get::<Option<String>>("", "graph_domain")
                .map_err(internal)?
                .as_deref()
                != Some("generic-v1")
            || !matches!(state.as_deref(), Some("RESERVED" | "LIVE"))
            || row
                .try_get::<Option<i16>>("", "lifetime_codec")
                .map_err(internal)?
                != Some(codec)
            || row
                .try_get::<Option<i32>>("", "lifetime_size")
                .map_err(internal)?
                != Some(size)
            || (state.as_deref() == Some("LIVE")
                && (graph.is_none()
                    || payload != current
                    || !row
                        .try_get::<bool>("", "payload_present")
                        .map_err(internal)?))
        {
            return Err(unavailable(
                "requested physical lifetime cannot be shared by legacy generic installation",
            ));
        }
    }
    Ok(())
}

fn member_digest(members: &BTreeMap<[u8; 32], (i32, Option<i64>)>) -> [u8; 32] {
    let mut hash = Sha256::new();
    for (id, (size, generation)) in members {
        hash.update(id);
        hash.update(size.to_be_bytes());
        match generation {
            None => hash.update([0]),
            Some(generation) => {
                hash.update([1]);
                hash.update(generation.to_be_bytes());
            }
        }
    }
    hash.finalize().into()
}

fn scope_bytes(scope: &PrimaryStorageScope) -> Result<Vec<u8>, SnapshotError> {
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
    if bytes.is_empty() || bytes.len() > 16384 {
        return Err(integrity("invalid primary install scope size"));
    }
    Ok(bytes)
}

fn seal(
    intent: &MetadataPrepareIntent,
    members: &[u8; 32],
    scope: &[u8],
) -> Result<[u8; 32], SnapshotError> {
    let id = uuid::Uuid::parse_str(&intent.prepare_id).map_err(internal)?;
    let mut hash = Sha256::new();
    hash.update(SEAL_DOMAIN);
    hash.update(id.as_bytes());
    hash.update((intent.operation_id.len() as u32).to_be_bytes());
    hash.update(intent.operation_id.as_bytes());
    hash.update(intent.manifest_digest);
    hash.update(members);
    hash.update((scope.len() as u32).to_be_bytes());
    hash.update(scope);
    Ok(hash.finalize().into())
}
