use std::{collections::BTreeMap, sync::Arc};

use mst2_codec::metapage::{Page, page_id};
use sea_orm::{QueryResult, Value};
use serde_json::json;

use super::*;
use crate::ceres::snapshot::{
    metadata_install::MetadataInstallIdentity,
    rooted_metadata_install::{RootedMetadataInstallPlan, RootedReuseRoot},
    rooted_metadata_projection::{CertifiedReusableDirectory, RootedReuseLookup},
};

#[path = "qualified_metadata_session.rs"]
mod sessions;

#[path = "qualified_metadata_gc.rs"]
mod gc;
#[path = "qualified_metadata_reader.rs"]
mod reader;
pub(crate) use reader::{RootedDirectoryWindow, RootedLookupBatch, RootedLookupStatus};
#[cfg(test)]
pub(crate) use reader::{
    with_rooted_reader_barriers, with_rooted_source_fact_barriers,
    with_rooted_source_temporary_shadow,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootedPrepareIntent {
    prepare_id: String,
    operation_id: String,
    plan: Arc<RootedMetadataInstallPlan>,
    bindings: BTreeMap<[u8; 32], (i64, u64)>,
    manifest_digest: [u8; 32],
    bindings_digest: [u8; 32],
    primary_scope: Vec<u8>,
    storage_seal: [u8; 32],
    root_generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootedMetadataReceipt {
    intent: RootedPrepareIntent,
    certificate_digest: [u8; 32],
    attestation_id: uuid::Uuid,
    attestation_digest: [u8; 32],
}

impl RootedPrepareIntent {
    #[cfg(test)]
    pub(crate) fn prepare_id(&self) -> &str {
        &self.prepare_id
    }
    #[cfg(test)]
    pub(crate) fn metadata_root(&self) -> [u8; 32] {
        self.plan.root
    }
    #[cfg(test)]
    pub(crate) fn root_generation(&self) -> i64 {
        self.root_generation
    }
}
impl RootedMetadataReceipt {
    pub(crate) fn metadata_root(&self) -> [u8; 32] {
        self.intent.plan.root
    }
}

pub(crate) struct RootedQualifiedMetadataRepository {
    connection: DatabaseConnection,
    namespace: VerifiedQualifiedNamespace,
    primary_scope: Vec<u8>,
    maintenance_state: tokio::sync::Mutex<gc::RootedMaintenanceState>,
}

#[async_trait::async_trait]
impl RootedReuseLookup for RootedQualifiedMetadataRepository {
    async fn lookup_reuse(
        &self,
        tree_oid: &str,
        identity: &MetadataInstallIdentity,
    ) -> Result<Option<CertifiedReusableDirectory>, SnapshotError> {
        // This is a bounded projection hint. It acquires no writer/retention
        // lock and grants no installation authority: begin_intent and finalize
        // independently bind the exact attestation and current lifetime.
        let kind = identity
            .tagged_root_tree_oid
            .split_once(':')
            .ok_or_else(|| integrity("rooted source identity has no tagged hash kind"))?
            .0;
        let profile = json!({"source_domain": identity.source_domain,"hash_kind": kind,
            "schema_version": identity.schema_version,"metadata_codec": identity.metadata_codec,
            "materialization_policy": identity.materialization_policy,"fs_semantics": identity.fs_semantics,
            "access_projection": identity.access_projection,"verification_revision": identity.verification_revision,
            "projection_revision": identity.projection_revision});
        let q = identifier(&self.namespace.schema);
        let c = identifier(&self.namespace.core_schema);
        let row = self.connection.query_one_raw(sql(format!(
            "SELECT a.root_page,a.root_generation,a.attestation_id::text,a.attestation_digest,
             p.certificate_digest,p.relative_path_bytes,p.relative_components,p.closure_nodes_upper,
             p.closure_edges_upper,p.closure_bytes_upper,p.closure_entries_upper
             FROM {q}.mst2_metadata_reuse_index i JOIN {q}.mst2_metadata_source_root_attestation a
               ON a.attestation_id=i.attestation_id AND a.root_page=i.root_page AND a.root_generation=i.root_generation
                 AND a.attestation_digest=i.attestation_digest
             JOIN {q}.mst2_metadata_page_certificate p ON p.page_id=a.root_page AND p.generation=a.root_generation
               AND p.certificate_digest=a.root_certificate_digest
             JOIN {q}.mst2_metadata_current cur ON cur.page_id=p.page_id AND cur.generation=p.generation
             JOIN {q}.mst2_metadata_lifetime life ON life.page_id=p.page_id AND life.generation=p.generation
             JOIN {q}.mst2_metadata_graph_node node ON node.page_id=p.page_id AND node.generation=p.generation
             JOIN {q}.mst2_metadata_payload body ON body.page_id=p.page_id AND body.generation=p.generation
             JOIN {q}.mst2_metadata_prepare origin ON origin.prepare_id=a.origin_prepare_id
             JOIN {c}.mega_tree source ON source.tree_id=split_part(a.tagged_tree_oid,':',2)
             WHERE i.tagged_tree_oid=$1 AND i.profile_digest=sha256(convert_to('mega.mst2.native-profile.v1','UTF8')
               ||decode('00','hex')||convert_to($2::jsonb::text,'UTF8')) AND a.source_profile=$2::jsonb
               AND a.namespace_uuid=$3::uuid AND {c}.mst2_route_source_tree_matches(split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest)
               AND life.state='LIVE' AND life.graph_domain='qualified-v1' AND node.state='LIVE'
               AND node.certificate_digest=p.certificate_digest AND node.bytes=p.byte_size
               AND body.byte_size=p.byte_size AND body.metadata_codec=p.metadata_codec
               AND life.expected_size=p.byte_size AND origin.state='COMMITTED' AND NOT pg_is_in_recovery()
               AND NOT EXISTS(SELECT 1 FROM {q}.mst2_metadata_gc_op gc WHERE gc.page_id=p.page_id AND gc.generation=p.generation)"
        ),[tree_oid.into(),profile.into(),self.namespace.namespace_uuid.clone().into()]))
            .await.map_err(database_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let count = |name: &str| -> Result<usize, SnapshotError> {
            usize::try_from(row.try_get::<i32>("", name).map_err(internal)?).map_err(internal)
        };
        let wide = |name: &str| -> Result<u64, SnapshotError> {
            u64::try_from(row.try_get::<i64>("", name).map_err(internal)?).map_err(internal)
        };
        Ok(Some(CertifiedReusableDirectory {
            page_id: digest_column(&row, "root_page")?,
            proof: RootedReuseRoot {
                generation: row.try_get("", "root_generation").map_err(internal)?,
                attestation_id: uuid::Uuid::parse_str(
                    &row.try_get::<String>("", "attestation_id")
                        .map_err(internal)?,
                )
                .map_err(internal)?,
                attestation_digest: digest_column(&row, "attestation_digest")?,
                certificate_digest: digest_column(&row, "certificate_digest")?,
            },
            relative_path_bytes: count("relative_path_bytes")?,
            relative_components: count("relative_components")?,
            closure_nodes_upper: count("closure_nodes_upper")?,
            closure_edges_upper: count("closure_edges_upper")?,
            closure_bytes_upper: wide("closure_bytes_upper")?,
            closure_entries_upper: usize::try_from(wide("closure_entries_upper")?)
                .map_err(internal)?,
        }))
    }
}

fn sql(text: impl Into<String>, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, text, values)
}
fn internal(error: impl std::fmt::Display) -> SnapshotError {
    SnapshotError::new(
        crate::ceres::snapshot::error::SnapshotErrorCode::Internal,
        error.to_string(),
    )
}
pub(super) fn is_lock_unavailable(error: &sea_orm::DbErr) -> bool {
    let runtime = match error {
        sea_orm::DbErr::Exec(runtime) | sea_orm::DbErr::Query(runtime) => runtime,
        _ => return false,
    };
    let sea_orm::RuntimeErr::SqlxError(sqlx_error) = runtime else {
        return false;
    };
    let sea_orm::sqlx::Error::Database(database_error) = sqlx_error.as_ref() else {
        return false;
    };
    database_error.code().as_deref() == Some("55P03")
}
fn database_error(error: sea_orm::DbErr) -> SnapshotError {
    if is_lock_unavailable(&error) {
        return SnapshotError::new(
            crate::ceres::snapshot::error::SnapshotErrorCode::TemporaryUnavailable,
            "qualified source is being updated; retry the operation",
        );
    }
    internal(error)
}
fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(
        crate::ceres::snapshot::error::SnapshotErrorCode::IntegrityError,
        message,
    )
}
fn unavailable(message: &str) -> SnapshotError {
    SnapshotError::new(
        crate::ceres::snapshot::error::SnapshotErrorCode::ObjectUnavailable,
        message,
    )
}
fn digest_column(row: &QueryResult, name: &str) -> Result<[u8; 32], SnapshotError> {
    let bytes: Vec<u8> = row.try_get("", name).map_err(internal)?;
    bytes.as_slice().try_into().map_err(internal)
}
fn encode_bindings(bindings: &BTreeMap<[u8; 32], (i64, u64)>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12 + 48 * bindings.len());
    bytes.extend_from_slice(b"MST2GEN1");
    bytes.extend_from_slice(&(bindings.len() as u32).to_be_bytes());
    for (page, (generation, size)) in bindings {
        bytes.extend_from_slice(page);
        bytes.extend_from_slice(&generation.to_be_bytes());
        bytes.extend_from_slice(&size.to_be_bytes());
    }
    bytes
}
fn decode_bindings(
    bytes: &[u8],
    plan: &RootedMetadataInstallPlan,
) -> Result<BTreeMap<[u8; 32], (i64, u64)>, SnapshotError> {
    if bytes.len() != 12 + 48 * plan.delta.len() || bytes.get(..8) != Some(b"MST2GEN1") {
        return Err(integrity(
            "rooted delta binding encoding differs from its plan",
        ));
    }
    let count = u32::from_be_bytes(bytes[8..12].try_into().map_err(internal)?) as usize;
    if count != plan.delta.len() {
        return Err(integrity(
            "rooted delta binding count differs from its plan",
        ));
    }
    let mut bindings = BTreeMap::new();
    for ((expected, size), record) in plan.delta.iter().zip(bytes[12..].as_chunks::<48>().0) {
        let page: [u8; 32] = record[..32].try_into().map_err(internal)?;
        let generation = i64::from_be_bytes(record[32..40].try_into().map_err(internal)?);
        let recorded_size = u64::from_be_bytes(record[40..48].try_into().map_err(internal)?);
        if page != *expected || recorded_size != *size || generation <= 0 {
            return Err(integrity("rooted exact delta lifetime binding is invalid"));
        }
        bindings.insert(page, (generation, recorded_size));
    }
    Ok(bindings)
}

async fn committed<T>(
    txn: DatabaseTransaction,
    result: Result<T, SnapshotError>,
    operation: &str,
    digest: [u8; 32],
    phase: super::super::native_metadata_install::MetadataCommitPhase,
) -> Result<T, MetadataInstallError> {
    match result {
        Err(error) => {
            let _ = txn.rollback().await;
            Err(error.into())
        }
        Ok(value) => match txn.commit().await {
            Ok(()) => Ok(value),
            Err(_) => Err(MetadataInstallError::CommitUncertain {
                operation_id: operation.into(),
                manifest_digest: digest,
                phase,
            }),
        },
    }
}

impl RootedQualifiedMetadataRepository {
    pub(crate) async fn open(
        core: &DatabaseConnection,
        config: &DbConfig,
    ) -> Result<Self, MegaError> {
        let captured = captured_core(core).await?;
        let namespace = registered(core, &captured)
            .await?
            .ok_or_else(|| rejected("rooted qualified family is not provisioned"))?;
        let mut q_config = config.clone();
        q_config.db_url = pool_url(&config.db_url, &namespace)?;
        q_config.max_connection = q_config.max_connection.clamp(1, 4);
        q_config.min_connection = 1;
        let connection = postgres_connection(&q_config).await?;
        let txn = connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await?;
        namespace
            .enter(&txn)
            .await
            .map_err(|e| rejected(&e.to_string()))?;
        let scope:serde_json::Value=txn.query_one_raw(sql(
            "SELECT jsonb_build_array(s.storage_uuid,current_database(),d.oid::bigint,current_schema(),n.oid::bigint,
             inet_server_addr()::text,inet_server_port()) AS scope FROM mst2_metadata_storage_scope s
             JOIN pg_catalog.pg_database d ON d.datname=current_database()
             JOIN pg_catalog.pg_namespace n ON n.nspname=current_schema() WHERE s.singleton=1 AND NOT pg_is_in_recovery()",[],
        )).await?.ok_or_else(||rejected("rooted captured primary scope is missing"))?.try_get("","scope")?;
        let primary_scope = serde_json::to_vec(&scope).map_err(|e| rejected(&e.to_string()))?;
        txn.commit().await?;
        Ok(Self {
            connection,
            namespace,
            primary_scope,
            maintenance_state: tokio::sync::Mutex::default(),
        })
    }

    async fn transaction(&self) -> Result<DatabaseTransaction, SnapshotError> {
        let txn = self
            .connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(database_error)?;
        self.namespace.enter(&txn).await?;
        let valid: bool = txn
            .query_one_raw(sql(
                "SELECT mst2_metadata_scope_matches($1) AS valid",
                [self.primary_scope.clone().into()],
            ))
            .await
            .map_err(database_error)?
            .ok_or_else(|| integrity("rooted primary scope is missing"))?
            .try_get("", "valid")
            .map_err(internal)?;
        if !valid {
            return Err(integrity("rooted writer left its captured primary scope"));
        }
        Ok(txn)
    }

    async fn load_intent<C: ConnectionTrait>(
        &self,
        db: &C,
        operation: &str,
        plan: &RootedMetadataInstallPlan,
    ) -> Result<Option<(RootedPrepareIntent, String)>, SnapshotError> {
        let Some(row)=db.query_one_raw(sql("SELECT prepare_id,plan_kind,state,manifest_digest,canonical_plan,canonical_bindings,
            bindings_digest,primary_scope,storage_seal FROM mst2_metadata_prepare WHERE operation_id=$1",[operation.into()]))
            .await.map_err(internal)? else {return Ok(None);};
        let digest = plan.digest()?;
        if row.try_get::<String>("", "plan_kind").map_err(internal)? != "ROOTED"
            || digest_column(&row, "manifest_digest")? != digest
        {
            return Err(SnapshotError::new(
                crate::ceres::snapshot::error::SnapshotErrorCode::Conflict,
                "operation is bound to a different rooted manifest",
            ));
        }
        let stored: Vec<u8> = row.try_get("", "canonical_plan").map_err(internal)?;
        if RootedMetadataInstallPlan::decode(&stored, &digest)? != *plan {
            return Err(integrity(
                "rooted stored plan differs from its exact source identity",
            ));
        }
        let encoded_bindings: Vec<u8> = row.try_get("", "canonical_bindings").map_err(internal)?;
        let bindings_digest: [u8; 32] = Sha256::digest(&encoded_bindings).into();
        if bindings_digest != digest_column(&row, "bindings_digest")? {
            return Err(integrity(
                "rooted stored lifetime binding digest is invalid",
            ));
        }
        let primary_scope: Vec<u8> = row.try_get("", "primary_scope").map_err(internal)?;
        if primary_scope != self.primary_scope {
            return Err(integrity(
                "rooted operation belongs to another physical primary",
            ));
        }
        let bindings = decode_bindings(&encoded_bindings, plan)?;
        let root_generation = bindings
            .get(&plan.root)
            .map(|b| b.0)
            .or_else(|| plan.reused.get(&plan.root).map(|b| b.generation))
            .ok_or_else(|| integrity("rooted root lifetime is missing"))?;
        Ok(Some((
            RootedPrepareIntent {
                prepare_id: row.try_get("", "prepare_id").map_err(internal)?,
                operation_id: operation.into(),
                plan: Arc::new(plan.clone()),
                bindings,
                manifest_digest: digest,
                bindings_digest,
                primary_scope,
                storage_seal: digest_column(&row, "storage_seal")?,
                root_generation,
            },
            row.try_get("", "state").map_err(internal)?,
        )))
    }

    pub(crate) async fn begin_intent(
        &self,
        operation: &str,
        plan: &RootedMetadataInstallPlan,
    ) -> Result<RootedPrepareIntent, MetadataInstallError> {
        plan.validate()?;
        if operation.is_empty() || operation.len() > 255 || operation.contains('\0') {
            return Err(integrity("invalid rooted operation ID").into());
        }
        let manifest_digest = plan.digest()?;
        let txn = self.transaction().await?;
        let result=async {
            if let Some((intent,state))=self.load_intent(&txn,operation,plan).await? {
                if state=="ABORTED" {return Err(unavailable("rooted preparation was definitively aborted"));}
                return Ok(intent);
            }
            let pages:Vec<_>=plan.delta.iter().map(|(page,size)|json!({"page":hex::encode(page),"size":size})).collect();
            let encoded=serde_json::to_string(&pages).map_err(internal)?;
            txn.execute_raw(sql("INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size,graph_domain)
                SELECT decode(p.page,'hex'),'page:sha256:'||p.page,coalesce(cur.generation+1,1),'RESERVED',1,p.size,'qualified-v1'
                FROM jsonb_to_recordset($1::jsonb) p(page text,size integer)
                LEFT JOIN mst2_metadata_current cur ON cur.page_id=decode(p.page,'hex')
                LEFT JOIN mst2_metadata_lifetime previous ON previous.page_id=cur.page_id AND previous.generation=cur.generation
                WHERE (cur.page_id IS NULL AND NOT EXISTS(SELECT 1 FROM mst2_metadata_lifetime life WHERE life.page_id=decode(p.page,'hex')))
                  OR (previous.state='REMOVED' AND cur.generation<9223372036854775807
                    AND EXISTS(SELECT 1 FROM mst2_metadata_gc_op proof WHERE proof.page_id=cur.page_id
                      AND proof.generation=cur.generation AND proof.state='APPLIED'))",
                [encoded.clone().into()])).await.map_err(internal)?;
            txn.execute_raw(sql("INSERT INTO mst2_metadata_current(page_id,generation)
                SELECT life.page_id,life.generation FROM jsonb_to_recordset($1::jsonb) p(page text,size integer)
                JOIN mst2_metadata_lifetime life ON life.page_id=decode(p.page,'hex') AND life.generation=1 AND life.state='RESERVED'
                WHERE NOT EXISTS(SELECT 1 FROM mst2_metadata_current cur WHERE cur.page_id=life.page_id)",
                [encoded.clone().into()])).await.map_err(internal)?;
            txn.execute_raw(sql("UPDATE mst2_metadata_current cur SET generation=fresh.generation
                FROM jsonb_to_recordset($1::jsonb) p(page text,size integer),mst2_metadata_lifetime previous,
                  mst2_metadata_lifetime fresh
                WHERE cur.page_id=decode(p.page,'hex') AND previous.page_id=cur.page_id AND previous.generation=cur.generation
                  AND previous.state='REMOVED' AND cur.generation<9223372036854775807
                  AND fresh.page_id=cur.page_id AND fresh.generation=cur.generation+1 AND fresh.state='RESERVED'
                  AND fresh.expected_size=p.size AND fresh.metadata_codec=1 AND fresh.graph_domain='qualified-v1'
                  AND EXISTS(SELECT 1 FROM mst2_metadata_gc_op proof WHERE proof.page_id=cur.page_id
                    AND proof.generation=cur.generation AND proof.state='APPLIED')",[encoded.clone().into()]))
                .await.map_err(internal)?;
            let rows=txn.query_all_raw(sql("SELECT cur.page_id,cur.generation,life.expected_size,life.metadata_codec,life.graph_domain,life.state,
                n.state AS graph_state,n.certificate_digest,body.byte_size FROM jsonb_to_recordset($1::jsonb) p(page text,size integer)
                JOIN mst2_metadata_current cur ON cur.page_id=decode(p.page,'hex') JOIN mst2_metadata_lifetime life USING(page_id,generation)
                LEFT JOIN mst2_metadata_graph_node n USING(page_id,generation) LEFT JOIN mst2_metadata_payload body USING(page_id,generation)
                WHERE NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=cur.page_id AND gc.generation=cur.generation)
                ORDER BY cur.page_id",[encoded.into()])).await.map_err(internal)?;
            let mut bindings=BTreeMap::new();
            for row in rows {
                let page=digest_column(&row,"page_id")?; let size=row.try_get::<i32>("","expected_size").map_err(internal)? as u64;
                let state:String=row.try_get("","state").map_err(internal)?;
                if plan.delta.get(&page)!=Some(&size) || row.try_get::<i16>("","metadata_codec").map_err(internal)?!=1
                    || row.try_get::<String>("","graph_domain").map_err(internal)?!="qualified-v1" || !matches!(state.as_str(),"RESERVED"|"LIVE")
                    || state=="LIVE" && (row.try_get::<Option<String>>("","graph_state").map_err(internal)?.as_deref()!=Some("LIVE")
                        || row.try_get::<Option<i32>>("","byte_size").map_err(internal)?!=Some(size as i32)) {
                    return Err(unavailable("rooted requested delta lifetime is not exactly installable"));
                }
                bindings.insert(page,(row.try_get("","generation").map_err(internal)?,size));
            }
            if bindings.len()!=plan.delta.len() {return Err(unavailable("rooted delta lifetime coverage is incomplete"));}
            let canonical_bindings=encode_bindings(&bindings); let bindings_digest:[u8;32]=Sha256::digest(&canonical_bindings).into();
            let prepare_id=uuid::Uuid::new_v4().to_string();
            let root_generation=bindings.get(&plan.root).map(|b|b.0).or_else(||plan.reused.get(&plan.root).map(|b|b.generation))
                .ok_or_else(||integrity("rooted root lifetime is missing"))?;
            let mut seal=Sha256::new(); seal.update(b"mega.mst2.rooted-storage-seal.v1\0");
            seal.update(prepare_id.as_bytes()); seal.update(manifest_digest); seal.update(bindings_digest);
            seal.update((self.primary_scope.len() as u64).to_be_bytes()); seal.update(&self.primary_scope);
            seal.update(plan.root); seal.update(root_generation.to_be_bytes());
            let storage_seal:[u8;32]=seal.finalize().into(); let identity=&plan.identity;
            txn.execute_raw(sql("INSERT INTO mst2_metadata_prepare(prepare_id,operation_id,manifest_digest,canonical_plan,plan_kind,
                source_domain,tagged_root_tree_oid,scope,schema_version,metadata_codec,materialization_policy,fs_semantics,access_projection,
                verification_revision,projection_revision,metadata_root,node_count,edge_count,total_bytes,state,canonical_bindings,bindings_digest,
                primary_scope,storage_seal,graph_domain) VALUES($1,$2,$3,$4,'ROOTED',$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,'PREPARING',$19,$20,$21,$22,'qualified-v1')",
                [prepare_id.clone().into(),operation.into(),manifest_digest.to_vec().into(),plan.encode()?.into(),identity.source_domain.clone().into(),
                 identity.tagged_root_tree_oid.clone().into(),identity.scope.clone().into(),(identity.schema_version as i16).into(),
                 (identity.metadata_codec as i16).into(),(identity.materialization_policy as i16).into(),(identity.fs_semantics as i16).into(),
                 (identity.access_projection as i16).into(),identity.verification_revision.into(),(identity.projection_revision as i16).into(),
                 plan.root.to_vec().into(),(plan.delta.len() as i32).into(),(plan.edges.len() as i32).into(),(plan.delta_bytes()? as i64).into(),
                 canonical_bindings.into(),bindings_digest.to_vec().into(),self.primary_scope.clone().into(),storage_seal.to_vec().into()])).await.map_err(internal)?;
            let delta:Vec<_>=bindings.iter().map(|(page,(generation,size))|json!({"page":hex::encode(page),"generation":generation,"size":size})).collect();
            txn.execute_raw(sql("INSERT INTO mst2_metadata_prepare_page(prepare_id,page_id,generation,expected_size)
                SELECT $1,decode(p.page,'hex'),p.generation,p.size FROM jsonb_to_recordset($2::jsonb) p(page text,generation bigint,size integer)",
                [prepare_id.clone().into(),serde_json::to_string(&delta).map_err(internal)?.into()])).await.map_err(internal)?;
            let reused:Vec<_>=plan.reused.iter().map(|(page,proof)|json!({"page":hex::encode(page),"generation":proof.generation,
                "attestation_id":proof.attestation_id.to_string(),"attestation_digest":hex::encode(proof.attestation_digest)})).collect();
            txn.execute_raw(sql("INSERT INTO mst2_metadata_prepare_reuse_root(prepare_id,root_page,root_generation,attestation_id,attestation_digest)
                SELECT $1,decode(r.page,'hex'),r.generation,r.attestation_id::uuid,decode(r.attestation_digest,'hex')
                FROM jsonb_to_recordset($2::jsonb) r(page text,generation bigint,attestation_id text,attestation_digest text)",
                [prepare_id.clone().into(),serde_json::to_string(&reused).map_err(internal)?.into()])).await.map_err(internal)?;
            txn.execute_raw(sql("INSERT INTO mst2_metadata_root_anchor(anchor_id,anchor_kind,owner_key,prepare_id,root_page,root_generation,root_certificate_digest)
                SELECT gen_random_uuid(),'REUSE',$1,$1,r.root_page,r.root_generation,a.root_certificate_digest
                FROM mst2_metadata_prepare_reuse_root r JOIN mst2_metadata_source_root_attestation a USING(attestation_id) WHERE r.prepare_id=$1",
                [prepare_id.clone().into()])).await.map_err(internal)?;
            Ok(RootedPrepareIntent {prepare_id,operation_id:operation.into(),plan:Arc::new(plan.clone()),bindings,manifest_digest,
                bindings_digest,primary_scope:self.primary_scope.clone(),storage_seal,root_generation})
        }.await;
        committed(
            txn,
            result,
            operation,
            manifest_digest,
            super::super::native_metadata_install::MetadataCommitPhase::Intent,
        )
        .await
    }

    async fn require_intent<C: ConnectionTrait>(
        &self,
        db: &C,
        expected: &RootedPrepareIntent,
    ) -> Result<String, SnapshotError> {
        let (stored, state) = self
            .load_intent(db, &expected.operation_id, &expected.plan)
            .await?
            .ok_or_else(|| unavailable("rooted preparation is missing"))?;
        if stored != *expected {
            return Err(integrity(
                "rooted preparation crossed its immutable physical binding",
            ));
        }
        if state == "ABORTED" {
            return Err(unavailable("rooted preparation was definitively aborted"));
        }
        Ok(state)
    }

    pub(crate) async fn install_pages(
        &self,
        intent: &RootedPrepareIntent,
        payloads: &[MetadataPagePayload],
    ) -> Result<(), MetadataInstallError> {
        if payloads.is_empty() || payloads.len() > 64 {
            return Err(integrity("rooted payload batch requires 1..=64 pages").into());
        }
        let mut pages = BTreeMap::new();
        for payload in payloads {
            let &(generation, size) = intent
                .bindings
                .get(&payload.id)
                .ok_or_else(|| integrity("rooted payload is outside its fixed delta"))?;
            if size != payload.size
                || payload.bytes.len() as u64 != size
                || page_id(&payload.bytes) != payload.id
            {
                return Err(integrity(
                    "rooted delta payload differs from its exact page digest or size",
                )
                .into());
            }
            Page::decode(&payload.bytes).map_err(internal)?;
            if pages
                .insert(
                    payload.id,
                    json!({"page":hex::encode(payload.id),"generation":generation,"size":size,
                "payload":hex::encode(&payload.bytes)}),
                )
                .is_some()
            {
                return Err(integrity("rooted payload batch has duplicate pages").into());
            }
        }
        let encoded =
            serde_json::to_string(&pages.into_values().collect::<Vec<_>>()).map_err(internal)?;
        let txn = self.transaction().await?;
        let result=async {
            let state=self.require_intent(&txn,intent).await?;
            if state=="PREPARING" {
                txn.execute_raw(sql("INSERT INTO mst2_metadata_payload(page_id,generation,metadata_codec,byte_size,payload)
                    SELECT decode(p.page,'hex'),p.generation,1,p.size,decode(p.payload,'hex')
                    FROM jsonb_to_recordset($1::jsonb) p(page text,generation bigint,size integer,payload text)
                    WHERE NOT EXISTS(SELECT 1 FROM mst2_metadata_payload body WHERE body.page_id=decode(p.page,'hex'))
                    ON CONFLICT(page_id) DO NOTHING",[encoded.clone().into()])).await.map_err(internal)?;
            }
            if txn.query_one_raw(sql("SELECT p.page FROM jsonb_to_recordset($1::jsonb) p(page text,generation bigint,size integer,payload text)
                LEFT JOIN mst2_metadata_payload body ON body.page_id=decode(p.page,'hex')
                LEFT JOIN mst2_metadata_current cur ON cur.page_id=body.page_id AND cur.generation=body.generation
                WHERE body.page_id IS NULL OR cur.page_id IS NULL OR body.generation<>p.generation OR body.metadata_codec<>1
                  OR body.byte_size<>p.size OR body.payload<>decode(p.payload,'hex') LIMIT 1",[encoded.into()]))
                .await.map_err(internal)?.is_some() {return Err(integrity("rooted durable delta payload conflicts with its exact incarnation"));}
            Ok(())
        }.await;
        committed(
            txn,
            result,
            &intent.operation_id,
            intent.manifest_digest,
            super::super::native_metadata_install::MetadataCommitPhase::Payload,
        )
        .await
    }

    pub(crate) async fn finalize(
        &self,
        intent: &RootedPrepareIntent,
    ) -> Result<RootedMetadataReceipt, MetadataInstallError> {
        // Rehash/decode the actual delta outside the core route lock. A cold
        // preparation additionally keeps the full Rust DAG validator as oracle.
        let payloads = self.read_delta(intent).await?;
        if intent.plan.reused.is_empty() {
            use crate::ceres::snapshot::retention_dag::{
                MetadataDagCandidate, MetadataDagLimits, ValidatedMetadataDag,
            };
            ValidatedMetadataDag::validate(
                MetadataDagCandidate {
                    metadata_codec: intent.plan.identity.metadata_codec,
                    root: intent.plan.root,
                    pages: payloads,
                    edges: intent.plan.edges.iter().copied().collect(),
                },
                MetadataDagLimits::default(),
            )?;
        }
        let txn = self.transaction().await?;
        let result=async {
            txn.query_one_raw(sql("SELECT prepare_id FROM mst2_metadata_prepare WHERE prepare_id=$1 FOR UPDATE",
                [intent.prepare_id.clone().into()])).await.map_err(internal)?;
            let state=self.require_intent(&txn,intent).await?;
            if state=="COMMITTED" {return self.receipt(&txn,intent).await;}
            let ordered:Vec<_>=intent.plan.child_first_delta()?.iter().map(|page|json!({"page":hex::encode(page),
                "generation":intent.bindings[page].0})).collect();
            if !ordered.is_empty() {
                let count:i32=txn.query_one_raw(sql("SELECT mst2_metadata_certify_batch($1,$2::jsonb) AS certified",
                    [intent.prepare_id.clone().into(),serde_json::to_string(&ordered).map_err(internal)?.into()]))
                    .await.map_err(internal)?.ok_or_else(||integrity("rooted certification result is missing"))?.try_get("","certified").map_err(internal)?;
                if count as usize!=ordered.len() {return Err(integrity("rooted certification did not cover its exact delta"));}
            }
            txn.execute_raw(sql("INSERT INTO mst2_metadata_root_anchor(anchor_id,anchor_kind,owner_key,prepare_id,root_page,root_generation,root_certificate_digest)
                SELECT gen_random_uuid(),'PREPARE',$1,$1,c.page_id,c.generation,c.certificate_digest
                FROM mst2_metadata_page_certificate c WHERE c.page_id=$2 AND c.generation=$3
                ON CONFLICT(anchor_kind,owner_key,root_page,root_generation) DO NOTHING",
                [intent.prepare_id.clone().into(),intent.plan.root.to_vec().into(),intent.root_generation.into()])).await.map_err(internal)?;
            let sources:Vec<_>=intent.plan.source_roots.iter().map(|(tree,page)|json!({"tree":tree,"page":hex::encode(page)})).collect();
            let sources=txn.query_all_raw(sql("SELECT source.tree,cur.page_id FROM jsonb_to_recordset($1::jsonb) source(tree text,page text)
                JOIN mst2_metadata_current cur ON cur.page_id=decode(source.page,'hex')
                JOIN mst2_metadata_page_certificate proof USING(page_id,generation) ORDER BY proof.rank,source.tree",
                [serde_json::to_string(&sources).map_err(internal)?.into()])).await.map_err(internal)?;
            if sources.len()!=intent.plan.source_roots.len() {return Err(unavailable("rooted source certificate order is incomplete"));}
            for source in sources {
                let tree:String=source.try_get("","tree").map_err(internal)?;
                let page=digest_column(&source,"page_id")?;
                let generation=intent.bindings.get(&page).map(|b|b.0).or_else(||intent.plan.reused.get(&page).map(|b|b.generation))
                    .ok_or_else(||integrity("rooted directory source has no exact lifetime"))?;
                let reused=txn.query_one_raw(sql("SELECT a.attestation_id FROM mst2_metadata_prepare_reuse_root r
                    JOIN mst2_metadata_source_root_attestation boundary ON boundary.attestation_id=r.attestation_id
                    JOIN mst2_metadata_source_root_attestation a ON a.root_page=r.root_page AND a.root_generation=r.root_generation
                      AND a.root_certificate_digest=boundary.root_certificate_digest
                    JOIN mst2_metadata_prepare origin ON origin.prepare_id=a.origin_prepare_id
                    JOIN mst2_metadata_current cur ON cur.page_id=a.root_page AND cur.generation=a.root_generation
                    JOIN mst2_metadata_lifetime life USING(page_id,generation)
                    JOIN $CORE$.mega_tree source ON source.tree_id=split_part(a.tagged_tree_oid,':',2)
                    WHERE r.prepare_id=$1 AND r.root_page=$2 AND r.root_generation=$3 AND a.tagged_tree_oid=$4
                      AND origin.state='COMMITTED' AND life.state='LIVE' AND a.source_profile=mst2_metadata_native_profile($1)
                      AND $CORE$.mst2_route_source_tree_matches(split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest)
                    ORDER BY a.attestation_id LIMIT 1".replace("$CORE$",&identifier(&self.namespace.core_schema)),
                    [intent.prepare_id.clone().into(),page.to_vec().into(),generation.into(),tree.clone().into()])).await.map_err(internal)?;
                if reused.is_some() {continue;}
                txn.execute_raw(sql("INSERT INTO mst2_metadata_source_root_attestation(attestation_id,namespace_uuid,origin_prepare_id,tagged_tree_oid,
                    source_profile,profile_digest,source_body_digest,root_page,root_generation,root_certificate_digest,source_proof,attestation_digest)
                    SELECT gen_random_uuid(),(proof->>'namespace')::uuid,$1,$2,proof->'source_profile',decode(proof->>'profile_digest','hex'),
                    decode(proof->>'source_body_digest','hex'),$3,$4,decode(proof->>'root_certificate','hex'),proof,decode(proof->>'attestation','hex')
                    FROM (SELECT mst2_metadata_compute_source_proof($1,$2,$3,$4) AS proof) input",
                    [intent.prepare_id.clone().into(),tree.clone().into(),page.to_vec().into(),generation.into()])).await.map_err(internal)?;
            }
            let changed=txn.execute_raw(sql("UPDATE mst2_metadata_prepare SET state='COMMITTED',committed_at=clock_timestamp()
                WHERE prepare_id=$1 AND state='PREPARING' AND storage_seal=$2",
                [intent.prepare_id.clone().into(),intent.storage_seal.to_vec().into()])).await.map_err(internal)?;
            if changed.rows_affected()!=1 {return Err(integrity("rooted finalize lost its exact prepare transition"));}
            txn.execute_raw(sql("UPDATE mst2_metadata_lifetime life SET state='LIVE' FROM mst2_metadata_prepare_page member
                WHERE member.prepare_id=$1 AND life.page_id=member.page_id AND life.generation=member.generation AND life.state='RESERVED'",
                [intent.prepare_id.clone().into()])).await.map_err(internal)?;
            txn.execute_raw(sql("INSERT INTO mst2_metadata_reuse_index(profile_digest,tagged_tree_oid,attestation_id,root_page,root_generation,attestation_digest)
                SELECT a.profile_digest,a.tagged_tree_oid,a.attestation_id,a.root_page,a.root_generation,a.attestation_digest
                FROM mst2_metadata_source_root_attestation a WHERE a.origin_prepare_id=$1
                ON CONFLICT(profile_digest,tagged_tree_oid) DO NOTHING",[intent.prepare_id.clone().into()])).await.map_err(internal)?;
            self.receipt(&txn,intent).await
        }.await;
        committed(
            txn,
            result,
            &intent.operation_id,
            intent.manifest_digest,
            super::super::native_metadata_install::MetadataCommitPhase::Finalize,
        )
        .await
    }

    async fn read_delta(
        &self,
        intent: &RootedPrepareIntent,
    ) -> Result<Vec<MetadataPagePayload>, SnapshotError> {
        let rows=self.connection.query_all_raw(sql("SELECT member.page_id,member.generation,member.expected_size,body.metadata_codec,
            body.byte_size,body.payload FROM mst2_metadata_prepare_page member JOIN mst2_metadata_payload body USING(page_id,generation)
            JOIN mst2_metadata_current cur USING(page_id,generation) WHERE member.prepare_id=$1 ORDER BY member.page_id LIMIT 4097",
            [intent.prepare_id.clone().into()])).await.map_err(internal)?;
        if rows.len() != intent.bindings.len() {
            return Err(unavailable("rooted durable delta is incomplete"));
        }
        let mut payloads = Vec::with_capacity(rows.len());
        for row in rows {
            let page = digest_column(&row, "page_id")?;
            let &(generation, size) = intent
                .bindings
                .get(&page)
                .ok_or_else(|| integrity("rooted durable delta has an extra member"))?;
            let bytes: Vec<u8> = row.try_get("", "payload").map_err(internal)?;
            if row.try_get::<i64>("", "generation").map_err(internal)? != generation
                || row.try_get::<i32>("", "expected_size").map_err(internal)? as u64 != size
                || row.try_get::<i32>("", "byte_size").map_err(internal)? as u64 != size
                || row.try_get::<i16>("", "metadata_codec").map_err(internal)? != 1
                || bytes.len() as u64 != size
                || page_id(&bytes) != page
            {
                return Err(integrity(
                    "rooted durable delta crossed its fixed byte and lifetime binding",
                ));
            }
            Page::decode(&bytes).map_err(internal)?;
            payloads.push(MetadataPagePayload {
                id: page,
                size,
                bytes,
            });
        }
        Ok(payloads)
    }

    async fn receipt<C: ConnectionTrait>(
        &self,
        db: &C,
        intent: &RootedPrepareIntent,
    ) -> Result<RootedMetadataReceipt, SnapshotError> {
        let row=db.query_one_raw(sql("SELECT c.certificate_digest,a.attestation_id::text,a.attestation_digest
            FROM mst2_metadata_prepare q JOIN mst2_metadata_current cur ON cur.page_id=q.metadata_root AND cur.generation=$2
            JOIN mst2_metadata_lifetime life USING(page_id,generation) JOIN mst2_metadata_graph_node n USING(page_id,generation)
            JOIN mst2_metadata_page_certificate c USING(page_id,generation)
            JOIN mst2_metadata_source_root_attestation a ON a.root_page=c.page_id AND a.root_generation=c.generation
            JOIN mst2_metadata_prepare origin ON origin.prepare_id=a.origin_prepare_id
            JOIN $CORE$.mega_tree source ON source.tree_id=split_part(a.tagged_tree_oid,':',2)
            WHERE q.prepare_id=$1 AND q.plan_kind='ROOTED' AND q.state='COMMITTED' AND life.state='LIVE' AND n.state='LIVE'
              AND n.certificate_digest=c.certificate_digest AND a.root_certificate_digest=c.certificate_digest
              AND origin.state='COMMITTED' AND a.source_profile=mst2_metadata_native_profile($1)
              AND a.tagged_tree_oid=mst2_metadata_rooted_scope_tree($1)
              AND $CORE$.mst2_route_source_tree_matches(split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest)
              AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=c.page_id AND gc.generation=c.generation)
              AND (q.coverage_retired_at IS NULL AND EXISTS(SELECT 1 FROM mst2_metadata_root_anchor anchor
                WHERE anchor.anchor_kind='PREPARE' AND anchor.prepare_id=q.prepare_id AND anchor.owner_key=q.prepare_id
                  AND anchor.root_page=c.page_id AND anchor.root_generation=c.generation
                  AND anchor.root_certificate_digest=c.certificate_digest) OR mst2_metadata_session_covers_prepare(q.prepare_id))
            ORDER BY (a.origin_prepare_id=q.prepare_id) DESC,a.attestation_id LIMIT 1".replace("$CORE$",&identifier(&self.namespace.core_schema)),
            [intent.prepare_id.clone().into(),intent.root_generation.into()])).await.map_err(internal)?
            .ok_or_else(||unavailable("rooted definitive receipt has no exact active canonical source root"))?;
        Ok(RootedMetadataReceipt {
            intent: intent.clone(),
            certificate_digest: digest_column(&row, "certificate_digest")?,
            attestation_id: uuid::Uuid::parse_str(
                &row.try_get::<String>("", "attestation_id")
                    .map_err(internal)?,
            )
            .map_err(internal)?,
            attestation_digest: digest_column(&row, "attestation_digest")?,
        })
    }

    pub(crate) async fn recover(
        &self,
        operation: &str,
        plan: &RootedMetadataInstallPlan,
    ) -> Result<Option<RootedMetadataReceipt>, SnapshotError> {
        let txn = self.transaction().await?;
        let Some((intent, state)) = self.load_intent(&txn, operation, plan).await? else {
            txn.commit().await.map_err(internal)?;
            return Ok(None);
        };
        let receipt = if state == "COMMITTED" {
            Some(self.receipt(&txn, &intent).await?)
        } else {
            None
        };
        txn.commit().await.map_err(internal)?;
        Ok(receipt)
    }
}
