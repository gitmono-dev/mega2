//! Asynchronous PostgreSQL retention graph repository (T06-B, spec 10 §6).
//!
//! This repository is not yet wired into the in-process snapshot runtime.
//! Graph mutations serialize on a schema-scoped transaction advisory lock;
//! callers can include them in a publication transaction. Each mutation uses
//! a savepoint, so a rejected group cannot leave partial data in that caller's
//! READ COMMITTED transaction. Bytes must be complete and verified before
//! retaining a node.
//!
//! A GC claim commits DELETING and a PENDING REMOVE intent together. A worker
//! may repeat the idempotent physical deletion after a crash, then call
//! `complete_gc`. The outgoing edges, child counters and APPLIED receipt commit
//! together, so replay cannot subtract references twice. No method here deletes
//! bytes, and shared Git raw deletion remains disabled until GitRetentionPort
//! is integrated. Physical workers and durable lease/publication wiring remain
//! separate integration work. Removed identities are tombstoned by their GC
//! receipts: rebuilding one requires future generation/fencing integration,
//! otherwise a stale physical worker could delete its newly reconstructed bytes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use serde_json::json;

use crate::{
    callisto::{mst2_retention_gc_op, mst2_retention_node},
    ceres::snapshot::{
        error::{SnapshotError, SnapshotErrorCode},
        retention::{NodeState, RetentionEdge, RetentionNode, RetentionRoot},
    },
};

const MAX_NODES: usize = 4096;
const MAX_EDGES: usize = 16_384;
const MAX_ROOTS: usize = 16;
const MAX_PENDING_BATCH: u64 = 1000;
pub(crate) const RETENTION_LOCK_KEY: i32 = 1_296_717_362;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcClaim {
    /// This transaction marked the node and recorded its pending operation.
    Marked,
    /// The same operation is committed and awaits physical deletion/ack.
    Pending,
    /// The same operation already removed the node and released its edges.
    Applied,
    /// A root, an incoming edge, another claim or absence prevents collection.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcCompletion {
    pub replayed: bool,
    /// Newly zero-reference children; roots must still be checked when claimed.
    pub zero_reference_children: Vec<String>,
}

#[derive(Clone)]
pub struct PostgresRetentionRepository {
    connection: DatabaseConnection,
}

impl PostgresRetentionRepository {
    pub fn new(connection: DatabaseConnection) -> Self {
        Self { connection }
    }

    pub async fn node(
        &self,
        id: &str,
    ) -> Result<Option<mst2_retention_node::Model>, SnapshotError> {
        mst2_retention_node::Entity::find_by_id(id.to_owned())
            .one(&self.connection)
            .await
            .map_err(internal)
    }

    /// Bounded replay scan. PENDING intents survive process/worker replacement.
    pub async fn pending_gc(
        &self,
        limit: u64,
    ) -> Result<Vec<mst2_retention_gc_op::Model>, SnapshotError> {
        if limit == 0 || limit > MAX_PENDING_BATCH {
            return Err(limit_error("pending GC batch must be 1..=1000"));
        }
        mst2_retention_gc_op::Entity::find()
            .filter(mst2_retention_gc_op::Column::State.eq("PENDING"))
            .order_by_asc(mst2_retention_gc_op::Column::CreatedAt)
            .order_by_asc(mst2_retention_gc_op::Column::OperationId)
            .limit(limit)
            .all(&self.connection)
            .await
            .map_err(internal)
    }

    pub async fn retain_group(
        &self,
        nodes: &[RetentionNode],
        edges: &[RetentionEdge],
        roots: &[RetentionRoot],
    ) -> Result<(), SnapshotError> {
        let txn = self.connection.begin().await.map_err(internal)?;
        let result = Self::retain_group_in_txn(&txn, nodes, edges, roots).await;
        finish(txn, result).await
    }

    /// Retain an immutable group atomically. Roots cover each supplied node;
    /// callers select that group rather than scanning the reachable graph.
    /// An existing parent's edges may only be replayed, not extended; a new
    /// identity describes a new graph.
    pub async fn retain_group_in_txn(
        txn: &DatabaseTransaction,
        nodes: &[RetentionNode],
        edges: &[RetentionEdge],
        roots: &[RetentionRoot],
    ) -> Result<(), SnapshotError> {
        let group = PreparedGroup::new(nodes, edges, roots)?;
        let savepoint = begin_graph(txn).await?;
        let result = retain_locked(&savepoint, &group).await;
        finish(savepoint, result).await
    }

    pub async fn release_root(&self, root: &RetentionRoot) -> Result<(), SnapshotError> {
        let txn = self.connection.begin().await.map_err(internal)?;
        let result = Self::release_root_in_txn(&txn, root).await;
        finish(txn, result).await
    }

    pub async fn release_root_in_txn(
        txn: &DatabaseTransaction,
        root: &RetentionRoot,
    ) -> Result<(), SnapshotError> {
        let (key, _) = root_identity(root)?;
        let savepoint = begin_graph(txn).await?;
        let result = savepoint
            .execute_raw(statement(
                "DELETE FROM mst2_retention_root WHERE root_key = $1",
                [key.into()],
            ))
            .await
            .map(|_| ())
            .map_err(internal);
        finish(savepoint, result).await
    }

    pub async fn mark_deleting(
        &self,
        operation_id: &str,
        node_id: &str,
    ) -> Result<GcClaim, SnapshotError> {
        let txn = self.connection.begin().await.map_err(internal)?;
        let result = Self::mark_deleting_in_txn(&txn, operation_id, node_id).await;
        finish(txn, result).await
    }

    /// The LIVE check, zero-reference check, CAS and replay intent share the
    /// retention mutation lock. Acquisition either precedes this CAS or fails.
    pub async fn mark_deleting_in_txn(
        txn: &DatabaseTransaction,
        operation_id: &str,
        node_id: &str,
    ) -> Result<GcClaim, SnapshotError> {
        validate_id(operation_id)?;
        validate_id(node_id)?;
        let savepoint = begin_graph(txn).await?;
        let result = mark_locked(&savepoint, operation_id, node_id).await;
        finish(savepoint, result).await
    }

    /// Acknowledge an idempotent, durable physical delete. Until this succeeds
    /// DELETING parents retain every child. This changes graph rows only.
    pub async fn complete_gc(&self, operation_id: &str) -> Result<GcCompletion, SnapshotError> {
        let txn = self.connection.begin().await.map_err(internal)?;
        let result = Self::complete_gc_in_txn(&txn, operation_id).await;
        finish(txn, result).await
    }

    pub async fn complete_gc_in_txn(
        txn: &DatabaseTransaction,
        operation_id: &str,
    ) -> Result<GcCompletion, SnapshotError> {
        validate_id(operation_id)?;
        let savepoint = begin_graph(txn).await?;
        let result = complete_locked(&savepoint, operation_id).await;
        finish(savepoint, result).await
    }
}

struct PreparedGroup {
    nodes_json: String,
    edges_json: String,
    roots_json: String,
}

impl PreparedGroup {
    fn new(
        nodes: &[RetentionNode],
        edges: &[RetentionEdge],
        roots: &[RetentionRoot],
    ) -> Result<Self, SnapshotError> {
        if nodes.len() > MAX_NODES || edges.len() > MAX_EDGES || roots.len() > MAX_ROOTS {
            return Err(limit_error("retention group exceeds bounded batch limits"));
        }
        let mut unique_nodes = BTreeMap::new();
        for node in nodes {
            validate_id(&node.id)?;
            if node.state != NodeState::Live {
                return Err(unavailable("retention acquisition requires LIVE nodes"));
            }
            let bytes = i64::try_from(node.bytes)
                .map_err(|_| limit_error("retention bytes exceed signed database range"))?;
            let definition = (node.kind.as_str(), bytes);
            if unique_nodes
                .insert(node.id.as_str(), definition)
                .is_some_and(|old| old != definition)
            {
                return Err(integrity("conflicting retention node definitions"));
            }
        }
        let mut unique_edges = BTreeSet::new();
        for edge in edges {
            validate_id(&edge.parent)?;
            validate_id(&edge.child)?;
            unique_edges.insert((edge.parent.as_str(), edge.child.as_str()));
        }
        reject_cycle(&unique_edges)?;
        let roots: BTreeSet<_> = roots.iter().map(root_identity).collect::<Result<_, _>>()?;
        Ok(Self {
            nodes_json: json!(
                unique_nodes
                    .iter()
                    .map(|(id, (kind, bytes))| {
                        json!({"node_id": id, "kind": kind, "bytes": bytes})
                    })
                    .collect::<Vec<_>>()
            )
            .to_string(),
            edges_json: json!(
                unique_edges
                    .iter()
                    .map(|(parent, child)| { json!({"parent_id": parent, "child_id": child}) })
                    .collect::<Vec<_>>()
            )
            .to_string(),
            roots_json: json!(
                roots
                    .iter()
                    .map(|(key, kind)| { json!({"root_key": key, "root_kind": kind}) })
                    .collect::<Vec<_>>()
            )
            .to_string(),
        })
    }
}

fn reject_cycle(edges: &BTreeSet<(&str, &str)>) -> Result<(), SnapshotError> {
    let mut incoming: BTreeMap<&str, usize> = BTreeMap::new();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for &(parent, child) in edges {
        incoming.entry(parent).or_default();
        *incoming.entry(child).or_default() += 1;
        children.entry(parent).or_default().push(child);
    }
    let mut ready: VecDeque<_> = incoming
        .iter()
        .filter_map(|(&id, &count)| (count == 0).then_some(id))
        .collect();
    let mut processed = 0;
    while let Some(id) = ready.pop_front() {
        processed += 1;
        if let Some(children) = children.get(id) {
            for child in children {
                if let Some(count) = incoming.get_mut(child) {
                    *count -= 1;
                    if *count == 0 {
                        ready.push_back(child);
                    }
                }
            }
        }
    }
    if processed != incoming.len() {
        return Err(integrity("retention graph must be acyclic"));
    }
    Ok(())
}

async fn retain_locked(
    txn: &DatabaseTransaction,
    group: &PreparedGroup,
) -> Result<(), SnapshotError> {
    if txn
        .query_one_raw(statement(
            "SELECT i.node_id FROM jsonb_to_recordset($1::jsonb) AS i(node_id text) \
             WHERE NOT EXISTS (SELECT 1 FROM mst2_retention_node n WHERE n.node_id = i.node_id) \
             AND EXISTS (SELECT 1 FROM mst2_retention_gc_op op WHERE op.node_id = i.node_id) LIMIT 1",
            [group.nodes_json.clone().into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(unavailable(
            "removed node identity requires generation-fenced reconstruction",
        ));
    }
    // Batch comparisons happen before any externally visible commit. An
    // identity never changes kind, size or DELETING state on replay.
    if txn
        .query_one_raw(statement(
            "SELECT n.node_id, n.state FROM mst2_retention_node n \
             JOIN jsonb_to_recordset($1::jsonb) AS i(node_id text, kind text, bytes bigint) \
             ON n.node_id = i.node_id \
             WHERE n.state <> 'LIVE' OR n.kind <> i.kind OR n.bytes <> i.bytes LIMIT 1",
            [group.nodes_json.clone().into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(unavailable(
            "existing node is DELETING or conflicts with immutable identity",
        ));
    }
    if txn
        .query_one_raw(statement(
            "SELECT e.parent_id FROM \
             jsonb_to_recordset($1::jsonb) AS e(parent_id text, child_id text) \
             JOIN mst2_retention_node n ON n.node_id = e.parent_id \
             WHERE NOT EXISTS (SELECT 1 FROM mst2_retention_edge old \
               WHERE old.parent_id = e.parent_id AND old.child_id = e.child_id) LIMIT 1",
            [group.edges_json.clone().into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(integrity(
            "an existing retention parent's edges are immutable",
        ));
    }
    txn.execute_raw(statement(
        "INSERT INTO mst2_retention_node (node_id, kind, state, bytes, incoming_refs, created_at) \
         SELECT node_id, kind, 'LIVE', bytes, 0, now() FROM \
         jsonb_to_recordset($1::jsonb) AS i(node_id text, kind text, bytes bigint) \
         ON CONFLICT (node_id) DO NOTHING",
        [group.nodes_json.clone().into()],
    ))
    .await
    .map_err(internal)?;
    if txn
        .query_one_raw(statement(
            "SELECT e.parent_id FROM \
             jsonb_to_recordset($1::jsonb) AS e(parent_id text, child_id text) \
             LEFT JOIN mst2_retention_node p ON p.node_id = e.parent_id \
             LEFT JOIN mst2_retention_node c ON c.node_id = e.child_id \
             WHERE p.node_id IS NULL OR c.node_id IS NULL \
               OR p.state <> 'LIVE' OR c.state <> 'LIVE' LIMIT 1",
            [group.edges_json.clone().into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(unavailable(
            "retention edge requires two known LIVE endpoints",
        ));
    }
    txn.execute_raw(statement(
        "WITH added AS ( \
           INSERT INTO mst2_retention_edge (parent_id, child_id, created_at) \
           SELECT parent_id, child_id, now() FROM \
           jsonb_to_recordset($1::jsonb) AS e(parent_id text, child_id text) \
           ON CONFLICT (parent_id, child_id) DO NOTHING RETURNING child_id \
         ), delta AS (SELECT child_id, count(*) AS refs FROM added GROUP BY child_id) \
         UPDATE mst2_retention_node n SET incoming_refs = n.incoming_refs + delta.refs \
         FROM delta WHERE n.node_id = delta.child_id",
        [group.edges_json.clone().into()],
    ))
    .await
    .map_err(internal)?;
    txn.execute_raw(statement(
        "INSERT INTO mst2_retention_root (node_id, root_key, root_kind, created_at) \
         SELECT n.node_id, r.root_key, r.root_kind, now() FROM \
         jsonb_to_recordset($1::jsonb) AS n(node_id text) CROSS JOIN \
         jsonb_to_recordset($2::jsonb) AS r(root_key text, root_kind text) \
         ON CONFLICT (node_id, root_key) DO NOTHING",
        [
            group.nodes_json.clone().into(),
            group.roots_json.clone().into(),
        ],
    ))
    .await
    .map_err(internal)?;
    Ok(())
}

async fn mark_locked(
    txn: &DatabaseTransaction,
    operation_id: &str,
    node_id: &str,
) -> Result<GcClaim, SnapshotError> {
    if let Some(op) = mst2_retention_gc_op::Entity::find_by_id(operation_id.to_owned())
        .one(txn)
        .await
        .map_err(internal)?
    {
        if op.node_id != node_id || op.operation != "REMOVE" {
            return Err(integrity("GC operation id is bound to different work"));
        }
        return match op.state.as_str() {
            "PENDING" => Ok(GcClaim::Pending),
            "APPLIED" => Ok(GcClaim::Applied),
            _ => Err(integrity("GC operation is not replayable")),
        };
    }
    let row = txn
        .query_one_raw(statement(
            "SELECT n.state, n.incoming_refs, \
               (SELECT count(*) FROM mst2_retention_edge e WHERE e.child_id = n.node_id) AS actual_refs \
             FROM mst2_retention_node n WHERE n.node_id = $1",
            [node_id.into()],
        ))
        .await
        .map_err(internal)?;
    let Some(row) = row else {
        return Ok(GcClaim::Unavailable);
    };
    let refs: i64 = row.try_get("", "incoming_refs").map_err(internal)?;
    let actual: i64 = row.try_get("", "actual_refs").map_err(internal)?;
    if refs != actual || refs < 0 {
        return Err(integrity("retention reference audit failed; GC stopped"));
    }
    if row.try_get::<String>("", "state").map_err(internal)? != "LIVE" || refs != 0 {
        return Ok(GcClaim::Unavailable);
    }
    let changed = txn
        .execute_raw(statement(
            "UPDATE mst2_retention_node n SET state = 'DELETING' \
             WHERE node_id = $1 AND state = 'LIVE' AND incoming_refs = 0 \
             AND NOT EXISTS (SELECT 1 FROM mst2_retention_root r WHERE r.node_id = n.node_id) \
             AND NOT EXISTS (SELECT 1 FROM mst2_retention_edge e WHERE e.child_id = n.node_id)",
            [node_id.into()],
        ))
        .await
        .map_err(internal)?;
    if changed.rows_affected() == 0 {
        return Ok(GcClaim::Unavailable);
    }
    txn.execute_raw(statement(
        "INSERT INTO mst2_retention_gc_op (operation_id, node_id, operation, state, attempts, created_at) \
         VALUES ($1, $2, 'REMOVE', 'PENDING', 0, now())",
        [operation_id.into(), node_id.into()],
    ))
    .await
    .map_err(internal)?;
    Ok(GcClaim::Marked)
}

async fn complete_locked(
    txn: &DatabaseTransaction,
    operation_id: &str,
) -> Result<GcCompletion, SnapshotError> {
    let op = mst2_retention_gc_op::Entity::find_by_id(operation_id.to_owned())
        .one(txn)
        .await
        .map_err(internal)?
        .ok_or_else(|| integrity("unknown GC operation"))?;
    if op.operation != "REMOVE" {
        return Err(integrity("GC operation is not a removal"));
    }
    if op.state == "APPLIED" {
        return Ok(GcCompletion {
            replayed: true,
            zero_reference_children: Vec::new(),
        });
    }
    if op.state != "PENDING" {
        return Err(integrity("GC operation is not replayable"));
    }
    let node = mst2_retention_node::Entity::find_by_id(op.node_id.clone())
        .one(txn)
        .await
        .map_err(internal)?
        .ok_or_else(|| integrity("pending GC node is missing"))?;
    if node.state != "DELETING" || node.incoming_refs != 0 {
        return Err(integrity("pending GC node is not unreferenced DELETING"));
    }
    if txn
        .query_one_raw(statement(
            "SELECT node_id FROM mst2_retention_root WHERE node_id = $1 \
         UNION ALL SELECT child_id FROM mst2_retention_edge WHERE child_id = $1 LIMIT 1",
            [op.node_id.clone().into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(integrity("pending GC node gained a reference; GC stopped"));
    }
    // Audit before subtracting. Even a damaged stored counter must not cause
    // a child's data to be treated as unreferenced.
    if txn.query_one_raw(statement(
        "SELECT c.node_id FROM mst2_retention_edge e \
         JOIN mst2_retention_node c ON c.node_id = e.child_id \
         WHERE e.parent_id = $1 AND (c.state <> 'LIVE' OR c.incoming_refs <= 0 OR \
           c.incoming_refs <> (SELECT count(*) FROM mst2_retention_edge i WHERE i.child_id = c.node_id)) \
         LIMIT 1",
        [op.node_id.clone().into()],
    )).await.map_err(internal)?.is_some() {
        return Err(integrity("child reference audit failed; GC stopped"));
    }
    let rows = txn.query_all_raw(statement(
        "WITH removed AS (DELETE FROM mst2_retention_edge WHERE parent_id = $1 RETURNING child_id), \
           delta AS (SELECT child_id, count(*) AS refs FROM removed GROUP BY child_id) \
         UPDATE mst2_retention_node n SET incoming_refs = n.incoming_refs - delta.refs \
         FROM delta WHERE n.node_id = delta.child_id RETURNING n.node_id, n.incoming_refs",
        [op.node_id.clone().into()],
    )).await.map_err(internal)?;
    let mut zero_reference_children = Vec::new();
    for row in rows {
        if row.try_get::<i64>("", "incoming_refs").map_err(internal)? == 0 {
            zero_reference_children.push(row.try_get("", "node_id").map_err(internal)?);
        }
    }
    zero_reference_children.sort();
    txn.execute_raw(statement(
        "DELETE FROM mst2_retention_node WHERE node_id = $1 AND state = 'DELETING'",
        [op.node_id.into()],
    ))
    .await
    .map_err(internal)?;
    txn.execute_raw(statement(
        "UPDATE mst2_retention_gc_op SET state = 'APPLIED', completed_at = now(), attempts = attempts + 1 \
         WHERE operation_id = $1 AND state = 'PENDING'",
        [operation_id.into()],
    )).await.map_err(internal)?;
    Ok(GcCompletion {
        replayed: false,
        zero_reference_children,
    })
}

async fn begin_graph(txn: &DatabaseTransaction) -> Result<DatabaseTransaction, SnapshotError> {
    if txn.get_database_backend() != DbBackend::Postgres {
        return Err(internal("retention repository requires PostgreSQL"));
    }
    let isolation = txn
        .query_one_raw(statement("SHOW transaction_isolation", []))
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("transaction isolation query returned no row"))?
        .try_get_by_index::<String>(0)
        .map_err(internal)?;
    if isolation != "read committed" {
        return Err(internal(
            "retention mutations require READ COMMITTED transaction isolation",
        ));
    }
    let savepoint = txn.begin().await.map_err(internal)?;
    let result = savepoint
        .execute_raw(statement(
            "SELECT pg_advisory_xact_lock($1, hashtext(current_schema()))",
            [RETENTION_LOCK_KEY.into()],
        ))
        .await
        .map_err(internal);
    if let Err(err) = result {
        return finish(savepoint, Err(err)).await;
    }
    Ok(savepoint)
}

async fn finish<T>(
    txn: DatabaseTransaction,
    result: Result<T, SnapshotError>,
) -> Result<T, SnapshotError> {
    match result {
        Ok(value) => {
            txn.commit().await.map_err(internal)?;
            Ok(value)
        }
        Err(err) => {
            txn.rollback().await.map_err(internal)?;
            Err(err)
        }
    }
}

fn statement<const N: usize>(sql: &str, values: [sea_orm::Value; N]) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn validate_id(id: &str) -> Result<(), SnapshotError> {
    if id.is_empty() || id.len() > 255 || id.contains('\0') {
        return Err(limit_error(
            "retention identifiers require 1..=255 bytes without NUL",
        ));
    }
    Ok(())
}

fn root_identity(root: &RetentionRoot) -> Result<(String, &'static str), SnapshotError> {
    let (id, kind) = match root {
        RetentionRoot::Lease(id) => (id, "lease"),
        RetentionRoot::Pin(id) => (id, "pin"),
        RetentionRoot::Prepare(id) => (id, "prepare"),
    };
    validate_id(id)?;
    let key = format!("{kind}:{id}");
    validate_id(&key)?;
    Ok((key, kind))
}

fn internal(error: impl std::fmt::Display) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Internal, error.to_string())
}
fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}
fn unavailable(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::ObjectUnavailable, message)
}
fn limit_error(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::LimitExceeded, message)
}

#[cfg(test)]
#[path = "mst2_retention_tests.rs"]
mod tests;
