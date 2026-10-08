//! Captured physical Q provisioning and the sealed rooted production repository.

use std::sync::OnceLock;

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, IsolationLevel, Statement,
    TransactionTrait,
};
use sha2::{Digest, Sha256};
use url::Url;

#[cfg(test)]
use super::native_metadata_install::generations::{
    GenerationMetadataReceipt, GenerationPrepareIntent,
    qualified::PostgresQualifiedMetadataRepository,
};
use super::{init::postgres_connection, native_metadata_install::MetadataInstallError};
#[cfg(test)]
use crate::ceres::snapshot::pages::PreparedNativeMetadataRetention;
use crate::{
    ceres::snapshot::{error::SnapshotError, retention_dag::MetadataPagePayload},
    common::errors::MegaError,
    config::DbConfig,
};

const FAMILY: &str = "v3-rooted-qualified-1";
const FAMILY_SQL: &str = include_str!("qualified_metadata_family.sql");
const CANONICAL_SQL: &str = include_str!("qualified_metadata_canonical.sql");
const CERTIFICATES_SQL: &str = include_str!("qualified_metadata_certificates.sql");
const ANCHORS_SQL: &str = include_str!("qualified_metadata_anchors.sql");
const SOURCE_READ_SQL: &str = include_str!("qualified_metadata_source_read.sql");
const SOURCE_REVISION_SQL: &str = include_str!("qualified_source_revision.sql");
const ROOTED_SQL: &str = include_str!("qualified_metadata_rooted.sql");
const SERVING_SQL: &str = include_str!("qualified_metadata_serving.sql");
const GC_SQL: &str = include_str!("qualified_metadata_gc.sql");
const READER_LIFECYCLE_SQL: &str = include_str!("qualified_metadata_reader_lifecycle.sql");

#[path = "qualified_metadata_rooted.rs"]
mod rooted;
pub(crate) use rooted::{RootedLookupStatus, RootedQualifiedMetadataRepository};
#[cfg(test)]
#[path = "qualified_metadata_reader_previous_fixture.rs"]
pub(crate) mod reader_previous_fixture;
#[cfg(test)]
pub(crate) use rooted::{
    RootedPrepareIntent, with_rooted_reader_barriers, with_rooted_source_fact_barriers,
    with_rooted_source_temporary_shadow,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotMetadataFamily {
    Generic,
    Rooted,
}

pub(crate) async fn select_snapshot_family(
    connection: &DatabaseConnection,
    identity: &str,
    lease: bool,
) -> Result<Option<SnapshotMetadataFamily>, SnapshotError> {
    let result = async {
        let txn = connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await?;
        let (schema, _) = captured_core(&txn).await?;
        let function = if lease {
            "mst2_route_family_for_lease"
        } else {
            "mst2_route_family_for_snapshot"
        };
        let rows = txn
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!(
                    "SELECT namespace_uuid::text,graph_domain FROM {}.{function}($1,$2)",
                    identifier(&schema)
                ),
                [identity.into(), schema.into()],
            ))
            .await?;
        let family = match rows.as_slice() {
            [] => None,
            [row] => Some(match row.try_get::<String>("", "graph_domain")?.as_str() {
                "generic-v1" => SnapshotMetadataFamily::Generic,
                "qualified-v1" => SnapshotMetadataFamily::Rooted,
                _ => {
                    return Err(rejected(
                        "snapshot route selected an unsupported physical family",
                    ));
                }
            }),
            _ => {
                return Err(rejected(
                    "snapshot route selected multiple physical families",
                ));
            }
        };
        txn.commit().await?;
        Ok::<_, MegaError>(family)
    }
    .await;
    result.map_err(|error| {
        if let MegaError::Db(db_error) = &error
            && rooted::is_lock_unavailable(db_error)
        {
            return SnapshotError::new(
                crate::ceres::snapshot::error::SnapshotErrorCode::TemporaryUnavailable,
                "fixed source is being updated; retry the operation",
            );
        }
        SnapshotError::new(
            crate::ceres::snapshot::error::SnapshotErrorCode::IntegrityError,
            error.to_string(),
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedQualifiedNamespace {
    core_schema: String,
    core_oid: i64,
    schema: String,
    schema_oid: i64,
    namespace_uuid: String,
    storage_uuid: String,
    catalog_fingerprint: Vec<u8>,
}

fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
pub(crate) fn implementation_fingerprint() -> Vec<u8> {
    static FINGERPRINT: OnceLock<[u8; 32]> = OnceLock::new();
    FINGERPRINT
        .get_or_init(|| {
            let mut hash = Sha256::new();
            hash.update(b"mega.mst2.rooted-qualified-implementation.v1\0");
            // Length-prefix every source component so source selection and proof
            // revisions cannot change while retaining an accepted physical stamp.
            for (name, bytes) in [
                ("family", FAMILY.as_bytes()),
                ("family-ddl", FAMILY_SQL.as_bytes()),
                ("canonical-proof-revision-1", CANONICAL_SQL.as_bytes()),
                ("indexed-source-read-revision-1", SOURCE_READ_SQL.as_bytes()),
                ("captured-source-revision-1", SOURCE_REVISION_SQL.as_bytes()),
                ("rooted-collector-revision-1", GC_SQL.as_bytes()),
                (
                    "bounded-reader-lifecycle-revision-1",
                    READER_LIFECYCLE_SQL.as_bytes(),
                ),
                ("typed-certificate-revision-1", CERTIFICATES_SQL.as_bytes()),
                (
                    "source-attestation-and-anchor-revision-1",
                    ANCHORS_SQL.as_bytes(),
                ),
                ("rooted-plan-and-bindings-revision-1", ROOTED_SQL.as_bytes()),
                (
                    "rooted-session-and-reader-revision-1",
                    SERVING_SQL.as_bytes(),
                ),
                (
                    "rooted-physical-route-revision-1",
                    include_bytes!("../migration/m20261008_000200_rooted_routes.sql").as_slice(),
                ),
                (
                    "authority-catalog-selector",
                    include_bytes!("qualified_family_catalog.sql").as_slice(),
                ),
                (
                    "normalized-family-shape",
                    include_bytes!("qualified_family_shape.sql").as_slice(),
                ),
                (
                    "initial-core-registration",
                    include_bytes!("../migration/m20261008_000200_rooted_qualified_family.sql")
                        .as_slice(),
                ),
            ] {
                hash.update((name.len() as u64).to_le_bytes());
                hash.update(name.as_bytes());
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(bytes);
            }
            hash.finalize().into()
        })
        .to_vec()
}
pub(crate) fn render_family(
    core_schema: &str,
    core_oid: i64,
    q_schema: &str,
    q_oid: i64,
    n_uuid: &str,
    s_uuid: &str,
) -> String {
    FAMILY_SQL
        .replace("$CANONICAL_SQL$", CANONICAL_SQL)
        .replace("$CERTIFICATES_SQL$", CERTIFICATES_SQL)
        .replace("$ANCHORS_SQL$", ANCHORS_SQL)
        .replace("$SOURCE_READ_SQL$", SOURCE_READ_SQL)
        .replace("$ROOTED_SQL$", ROOTED_SQL)
        .replace("$SERVING_SQL$", SERVING_SQL)
        .replace("$GC_SQL$", GC_SQL)
        .replace("$READER_LIFECYCLE_SQL$", READER_LIFECYCLE_SQL)
        .replace("$CORE_SCHEMA$", &identifier(core_schema))
        .replace("$CORE_LITERAL$", &literal(core_schema))
        .replace("$Q_SCHEMA$", &identifier(q_schema))
        .replace("$Q_LITERAL$", &literal(q_schema))
        .replace("$CORE_OID$", &core_oid.to_string())
        .replace("$Q_OID$", &q_oid.to_string())
        .replace("$NAMESPACE_UUID$", n_uuid)
        .replace("$STORAGE_UUID$", s_uuid)
        .replace(
            "$IMPLEMENTATION_SHA$",
            &hex::encode(implementation_fingerprint()),
        )
        .replace("$HEADER_LEN$", &HEADER_LEN.to_string())
        .replace("$PAGE_MAX_BYTES$", &PAGE_MAX_BYTES.to_string())
}
fn rejected(message: &str) -> MegaError {
    MegaError::Other(message.into())
}

async fn captured_core<C: ConnectionTrait>(connection: &C) -> Result<(String, i64), MegaError> {
    let row = connection.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT current_schema() AS schema,n.oid::bigint AS oid FROM pg_catalog.pg_namespace n WHERE n.nspname=current_schema()"))
        .await?.ok_or_else(|| rejected("qualified family core schema is missing"))?;
    let schema: String = row.try_get("", "schema")?;
    let oid: i64 = row.try_get("", "oid")?;
    let valid = connection
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT {}.mst2_route_scope_valid($1) AS valid",
                identifier(&schema)
            ),
            [schema.clone().into()],
        ))
        .await?
        .ok_or_else(|| rejected("qualified family core identity is missing"))?
        .try_get::<bool>("", "valid")?;
    if !valid {
        return Err(rejected(
            "qualified family requires its registered primary core schema",
        ));
    }
    Ok((schema, oid))
}

async fn catalog<C: ConnectionTrait>(
    connection: &C,
    core_oid: i64,
    q_oid: i64,
) -> Result<Vec<u8>, MegaError> {
    catalog_with_exemption(connection, core_oid, q_oid, 0).await
}

async fn catalog_with_exemption<C: ConnectionTrait>(
    connection: &C,
    core_oid: i64,
    q_oid: i64,
    exempt_q_oid: i64,
) -> Result<Vec<u8>, MegaError> {
    // Inspect the actual catalogs directly. A replaced helper function cannot
    // turn a bad physical structure into a fresh accepted fingerprint.
    let sql = include_str!("qualified_family_catalog.sql")
        .replace("$CORE_OID$", "$1::bigint::oid")
        .replace("$Q_OID$", "$2::bigint::oid")
        .replace("$EXEMPT_Q_OID$", "$3::bigint::oid");
    let row = connection
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            [core_oid.into(), q_oid.into(), exempt_q_oid.into()],
        ))
        .await?
        .ok_or_else(|| rejected("qualified family catalog is missing"))?;
    Ok(row.try_get("", "fingerprint")?)
}

async fn registered<C: ConnectionTrait>(
    connection: &C,
    core: &(String, i64),
) -> Result<Option<VerifiedQualifiedNamespace>, MegaError> {
    let policy=connection.query_one_raw(Statement::from_string(DbBackend::Postgres,format!(
        "SELECT implementation_fingerprint,expected_shape,authority_catalog FROM {}.mst2_qualified_family_policy WHERE singleton=1",identifier(&core.0))))
        .await?.ok_or_else(||rejected("qualified trusted family policy is missing"))?;
    if policy.try_get::<Vec<u8>>("", "implementation_fingerprint")? != implementation_fingerprint()
    {
        return Err(rejected("qualified trusted core authority catalog changed"));
    }
    let rows=connection.query_all_raw(Statement::from_string(DbBackend::Postgres,format!(
        "SELECT n.namespace_uuid::text,n.metadata_schema,n.metadata_schema_oid::bigint,n.metadata_storage_uuid,
         n.implementation_fingerprint,n.catalog_fingerprint,
         n.family_identity,n.admission_state,n.collector_state,n.storage_uuid AS core_storage_uuid,
         n.core_schema,n.core_schema_oid::bigint,
         (SELECT count(*) FROM {c}.mst2_metadata_namespace)::bigint AS namespace_count,
         EXISTS(SELECT 1 FROM pg_catalog.pg_namespace p WHERE p.oid=n.metadata_schema_oid AND p.nspname=n.metadata_schema) AS schema_present,
         {c}.mst2_route_scope_valid({core_literal}) AS core_valid
         FROM {c}.mst2_metadata_namespace n WHERE n.graph_domain='qualified-v1'",c=identifier(&core.0),core_literal=literal(&core.0))))
        .await?;
    if rows.is_empty() {
        if policy.try_get::<Vec<u8>>("", "authority_catalog")?
            != catalog(connection, core.1, 0).await?
        {
            return Err(rejected("qualified trusted core authority catalog changed"));
        }
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(rejected(
            "qualified family registry exceeds its one-Q hard limit",
        ));
    }
    let row = &rows[0];
    let schema: String = row.try_get("", "metadata_schema")?;
    let namespace_uuid: String = row.try_get("", "namespace_uuid")?;
    let storage_uuid: String = row.try_get("", "metadata_storage_uuid")?;
    let schema_oid: i64 = row.try_get("", "metadata_schema_oid")?;
    let expected_schema = format!("mst2q_{}", namespace_uuid.replace('-', ""));
    let namespace_identity = uuid::Uuid::parse_str(&namespace_uuid)
        .map_err(|_| rejected("qualified namespace UUID is invalid"))?;
    let storage_identity = uuid::Uuid::parse_str(&storage_uuid)
        .map_err(|_| rejected("qualified storage UUID is invalid"))?;
    if schema != expected_schema
        || namespace_identity.get_version_num() != 4
        || namespace_identity.get_variant() != uuid::Variant::RFC4122
        || namespace_identity.to_string() != namespace_uuid
        || storage_identity.get_version_num() != 4
        || storage_identity.get_variant() != uuid::Variant::RFC4122
        || storage_identity.to_string() != storage_uuid
        || storage_uuid == row.try_get::<String>("", "core_storage_uuid")?
        || row.try_get::<String>("", "core_schema")? != core.0
        || row.try_get::<i64>("", "core_schema_oid")? != core.1
        || row.try_get::<String>("", "family_identity")? != FAMILY
        || row.try_get::<String>("", "admission_state")? != "ROOTED_Q_ADMITTED"
        || row.try_get::<String>("", "collector_state")? != "ENABLED"
        || row.try_get::<i64>("", "namespace_count")? != 2
        || !row.try_get::<bool>("", "schema_present")?
        || !row.try_get::<bool>("", "core_valid")?
        || row.try_get::<Vec<u8>>("", "implementation_fingerprint")? != implementation_fingerprint()
    {
        return Err(rejected(
            "qualified family registry and physical identity disagree",
        ));
    }
    // A single registered, physically present Q identity is the only permitted
    // source of core-side RI triggers omitted from the core authority stamp.
    // The complete catalog and trusted shape below still bind all of them.
    if policy.try_get::<Vec<u8>>("", "authority_catalog")?
        != catalog_with_exemption(connection, core.1, 0, schema_oid).await?
    {
        return Err(rejected("qualified trusted core authority catalog changed"));
    }
    let fingerprint: Vec<u8> = row.try_get("", "catalog_fingerprint")?;
    if fingerprint.len() != 32 || catalog(connection, core.1, schema_oid).await? != fingerprint {
        return Err(rejected(
            "qualified family actual catalog fingerprint changed",
        ));
    }
    let shape: Vec<u8> = connection
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT {}.mst2_route_family_shape($1::bigint::oid,$2::uuid,$3) AS fingerprint",
                identifier(&core.0)
            ),
            [
                schema_oid.into(),
                namespace_uuid.clone().into(),
                storage_uuid.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| rejected("qualified family actual shape is missing"))?
        .try_get("", "fingerprint")?;
    if shape != policy.try_get::<Vec<u8>>("", "expected_shape")? {
        return Err(rejected(
            "qualified namespace does not have the trusted complete physical family shape",
        ));
    }
    let stamp=connection.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
        "SELECT EXISTS(SELECT 1 FROM {q}.mst2_metadata_family_identity i JOIN {q}.mst2_metadata_storage_scope s USING(singleton)
         WHERE i.singleton=1 AND i.namespace_uuid=$1::uuid AND i.storage_uuid=$2 AND s.storage_uuid=$2
         AND i.core_schema_oid=$3::bigint::oid AND i.metadata_schema_oid=$4::bigint::oid
         AND i.family_identity=$5 AND i.implementation_fingerprint=$6) AS valid",q=identifier(&schema)),
        [namespace_uuid.clone().into(),storage_uuid.clone().into(),core.1.into(),schema_oid.into(),FAMILY.into(),implementation_fingerprint().into()]))
        .await?.ok_or_else(||rejected("qualified family physical stamp is missing"))?;
    if !stamp.try_get::<bool>("", "valid")? {
        return Err(rejected("qualified family physical stamp changed"));
    }
    Ok(Some(VerifiedQualifiedNamespace {
        core_schema: core.0.clone(),
        core_oid: core.1,
        schema,
        schema_oid,
        namespace_uuid,
        storage_uuid,
        catalog_fingerprint: fingerprint,
    }))
}

/// Production bootstrap calls this after core migrations, before returning a
/// writable app connection. Existing registrations are verified, never reset.
pub(crate) async fn provision_or_verify_rooted_qualified_family(
    connection: &DatabaseConnection,
) -> Result<VerifiedQualifiedNamespace, MegaError> {
    let core = captured_core(connection).await?;
    for attempt in 0..2 {
        let txn = connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await?;
        if let Some(namespace) = registered(&txn, &core).await? {
            txn.execute_unprepared(&format!(
                "SELECT {}.mst2_route_enter({})",
                identifier(&core.0),
                literal(&core.0)
            ))
            .await?;
            let locked = registered(&txn, &core)
                .await?
                .ok_or_else(|| rejected("qualified registration disappeared"))?;
            if locked != namespace {
                return Err(rejected("qualified registration changed during bootstrap"));
            }
            txn.commit().await?;
            return Ok(locked);
        }
        let candidate = uuid::Uuid::new_v4().to_string();
        let schema = format!("mst2q_{}", candidate.replace('-', ""));
        let enter = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!(
                    "SELECT {}.mst2_route_family_candidate_enter($1,$2::uuid,$3)",
                    identifier(&core.0)
                ),
                [
                    core.0.clone().into(),
                    candidate.clone().into(),
                    schema.clone().into(),
                ],
            ))
            .await;
        if let Err(error) = enter {
            txn.rollback().await?;
            if attempt == 0 && error.to_string().contains("lost bootstrap serialization") {
                continue;
            }
            return Err(error.into());
        }
        txn.execute_unprepared(&format!("CREATE SCHEMA {}", identifier(&schema)))
            .await?;
        let row = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT oid::bigint AS oid FROM pg_catalog.pg_namespace WHERE nspname=$1",
                [schema.clone().into()],
            ))
            .await?
            .ok_or_else(|| rejected("qualified provisioning schema was not created"))?;
        let schema_oid: i64 = row.try_get("", "oid")?;
        let storage_uuid = uuid::Uuid::new_v4().to_string();
        let sql = render_family(
            &core.0,
            core.1,
            &schema,
            schema_oid,
            &candidate,
            &storage_uuid,
        );
        txn.execute_unprepared(&sql).await?;
        let fingerprint = catalog(&txn, core.1, schema_oid).await?;
        txn.execute_unprepared(&format!(
            "SET LOCAL search_path={},pg_catalog,pg_temp",
            identifier(&core.0)
        ))
        .await?;
        txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
            "INSERT INTO {c}.mst2_metadata_namespace(singleton,namespace_uuid,core_schema,core_schema_oid,
             database_name,database_oid,storage_uuid,server_address,server_port,mono_lock_key2,metadata_schema,
             metadata_schema_oid,family_identity,graph_domain,admission_state,collector_state,
             metadata_storage_uuid,implementation_fingerprint,catalog_fingerprint)
             SELECT NULL,$1::uuid,g.core_schema,g.core_schema_oid,g.database_name,g.database_oid,g.storage_uuid,
             g.server_address,g.server_port,g.mono_lock_key2,$2,$3::bigint::oid,$4,'qualified-v1','ROOTED_Q_ADMITTED','ENABLED',$5,$6,$7
             FROM {c}.mst2_metadata_namespace g WHERE singleton=1",c=identifier(&core.0)),
            [candidate.into(),schema.into(),schema_oid.into(),FAMILY.into(),storage_uuid.into(),implementation_fingerprint().into(),fingerprint.into()])).await?;
        let namespace = registered(&txn, &core)
            .await?
            .ok_or_else(|| rejected("qualified provisioning did not register its family"))?;
        txn.commit().await?;
        return Ok(namespace);
    }
    Err(rejected(
        "qualified provisioning could not serialize bootstrap",
    ))
}

impl VerifiedQualifiedNamespace {
    pub(crate) async fn enter(&self, txn: &DatabaseTransaction) -> Result<(), SnapshotError> {
        let result: Result<(), MegaError> = async {
            let actual = txn
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SELECT current_schema() AS schema",
                ))
                .await?
                .ok_or_else(|| rejected("qualified writer schema is missing"))?
                .try_get::<String>("", "schema")?;
            if actual != self.schema {
                return Err(rejected(
                    "qualified writer escaped its physical pool schema",
                ));
            }
            txn.execute_unprepared(&format!(
                "SELECT {}.mst2_route_enter({})",
                identifier(&self.core_schema),
                literal(&self.core_schema)
            ))
            .await?;
            if registered(txn, &(self.core_schema.clone(), self.core_oid))
                .await?
                .as_ref()
                != Some(self)
            {
                return Err(rejected("qualified writer physical namespace changed"));
            }
            Ok(())
        }
        .await;
        result.map_err(|e| {
            if let MegaError::Db(error) = &e
                && rooted::is_lock_unavailable(error)
            {
                return SnapshotError::new(
                    crate::ceres::snapshot::error::SnapshotErrorCode::TemporaryUnavailable,
                    "qualified source is being updated; retry the operation",
                );
            }
            SnapshotError::new(
                crate::ceres::snapshot::error::SnapshotErrorCode::SnapshotNotReady,
                e.to_string(),
            )
        })
    }
}

fn pool_url(db_url: &str, namespace: &VerifiedQualifiedNamespace) -> Result<String, MegaError> {
    let mut url = Url::parse(db_url)
        .map_err(|_| rejected("qualified writer requires a valid PostgreSQL URL"))?;
    let mut options = Vec::new();
    let mut others = Vec::new();
    for (key, value) in url.query_pairs() {
        if key == "options" {
            options.push(value.into_owned());
        } else {
            others.push((key.into_owned(), value.into_owned()));
        }
    }
    // Only server-generated lowercase schema names can reach this option.
    options.push(format!(
        "-csearch_path={},pg_catalog,pg_temp",
        namespace.schema
    ));
    url.set_query(None);
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in others {
            pairs.append_pair(&key, &value);
        }
        pairs.append_pair("options", &options.join(" "));
    }
    Ok(url.to_string())
}

/// The adapter has no Deref or unsealed connection accessor. It permits only
/// test-only cold preparation writes, without a production factory.
#[cfg(test)]
pub(crate) struct ShadowQualifiedMetadataWriter {
    repository: PostgresQualifiedMetadataRepository,
}
#[cfg(test)]
impl ShadowQualifiedMetadataWriter {
    pub(crate) async fn open(
        core: &DatabaseConnection,
        config: &DbConfig,
    ) -> Result<Self, MegaError> {
        let captured = captured_core(core).await?;
        let namespace = registered(core, &captured)
            .await?
            .ok_or_else(|| rejected("qualified shadow family is not provisioned at bootstrap"))?;
        let mut q_config = config.clone();
        q_config.db_url = pool_url(&config.db_url, &namespace)?;
        q_config.max_connection = q_config.max_connection.clamp(1, 2);
        q_config.min_connection = 1;
        let connection = postgres_connection(&q_config).await?;
        let txn = connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await?;
        namespace
            .enter(&txn)
            .await
            .map_err(|e| rejected(&e.to_string()))?;
        txn.commit().await?;
        let repository =
            PostgresQualifiedMetadataRepository::registered_shadow(connection, namespace)
                .await
                .map_err(|e| rejected(&e.to_string()))?;
        Ok(Self { repository })
    }
    pub(crate) async fn begin_intent(
        &self,
        operation_id: &str,
        prepared: &PreparedNativeMetadataRetention,
    ) -> Result<GenerationPrepareIntent, MetadataInstallError> {
        self.repository.begin_intent(operation_id, prepared).await
    }
    pub(crate) async fn install_pages(
        &self,
        intent: &GenerationPrepareIntent,
        payloads: &[MetadataPagePayload],
    ) -> Result<(), MetadataInstallError> {
        self.repository.install_pages(intent, payloads).await
    }
    pub(crate) async fn finalize(
        &self,
        intent: &GenerationPrepareIntent,
    ) -> Result<GenerationMetadataReceipt, MetadataInstallError> {
        self.repository.finalize(intent).await
    }
}

#[cfg(test)]
#[path = "qualified_metadata_family_tests.rs"]
mod tests;
