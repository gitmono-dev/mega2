//! Native `/` publication certificates; origin path receipts retain their v1 identity.

use git_internal::hash::{ObjectHash, get_hash_kind};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseTransaction,
    EntityTrait, QueryFilter, QueryResult, Statement, TransactionTrait,
};

use crate::{
    callisto::{mega_refs, mst2_native_publication, mst2_publication, push_queue},
    common::utils::MEGA_BRANCH_NAME,
    jupiter::storage::{
        base_storage::StorageConnector,
        mono_storage::MonoStorage,
        mst2_publication_storage::{CommittedPublication, PublicationReceiptError},
        push_queue_storage::PushQueueStorage,
    },
};

const NATIVE_EPOCH: i64 = 1;

// Used by both ordinary reads and the single UPDATE that captures a B2.5 token.
pub(crate) const NATIVE_OBSERVATION_CTES: &str = r#"
    native_roots AS (
      SELECT count(*)::bigint AS root_count,
             max(ref_commit_hash) AS root_commit, max(ref_tree_hash) AS root_tree
        FROM mega_refs WHERE path = '/' AND ref_name = $1 AND is_cl = false
    ), native_observation AS (
      SELECT roots.*, h.instance_id, h.sequence, h.writer_epoch, h.state,
             h.root_commit AS head_commit, h.root_tree AS head_tree,
             h.certificate_receipt_id,
             c.receipt_id AS certificate_id, c.namespace AS certificate_namespace,
             c.instance_id AS certificate_instance, c.sequence AS certificate_sequence,
             c.writer_epoch AS certificate_epoch,
             c.old_root_commit AS certificate_old_commit,
             c.root_commit AS certificate_commit, c.root_tree AS certificate_tree,
             c.origin_path, c.origin_ref, c.old_path_commit, c.path_commit,
             r.id AS receipt_id, r.namespace AS receipt_namespace,
             r.sequence AS receipt_sequence, r.operation_id,
             r.old_oid, r.new_oid, r.writer_epoch AS receipt_epoch, r.writer_kind,
             r.request_digest, r.request_digest_version, r.native_certificate_version,
             o.id AS outbox_id, o.namespace AS outbox_namespace, o.sequence AS outbox_sequence
        FROM native_roots roots
        LEFT JOIN mst2_native_head h ON h.namespace = '/'
        LEFT JOIN mst2_native_publication c ON c.receipt_id = h.certificate_receipt_id
        LEFT JOIN mst2_publication r ON r.id = c.receipt_id
        LEFT JOIN mst2_publication_outbox o ON o.operation_id = r.operation_id
    )
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeRoot {
    pub(crate) commit: String,
    pub(crate) tree: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativePublicationToken {
    pub(crate) sequence: i64,
    pub(crate) epoch: i64,
    pub(crate) certificate: Option<i64>,
}

#[derive(Clone, Debug)]
pub(crate) struct NativePublicationHead {
    pub(crate) root: NativeRoot,
    pub(crate) instance_id: String,
    pub(crate) token: NativePublicationToken,
    ready: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeObservation {
    pub(crate) root: Option<NativeRoot>,
    pub(crate) head: Option<NativePublicationHead>,
}

#[derive(Debug)]
pub(crate) struct PreparedNativePublication {
    head: NativePublicationHead,
    path: String,
    old_path: Option<NativeRoot>,
    transaction_id: i64,
}

fn integrity(message: &str) -> PublicationReceiptError {
    PublicationReceiptError::Integrity(message.to_owned())
}

fn canonical_instance(instance: &str) -> Result<String, PublicationReceiptError> {
    uuid::Uuid::parse_str(instance)
        .map(|id| id.to_string())
        .map_err(|_| integrity("invalid native deployment instance"))
}

fn validate_root(root: &NativeRoot) -> Result<(), PublicationReceiptError> {
    for oid in [&root.commit, &root.tree] {
        ObjectHash::from_hex_for_kind(get_hash_kind(), oid)
            .map_err(|_| integrity("invalid native object identity"))?;
    }
    Ok(())
}

pub(crate) fn decode_native_observation(
    row: &QueryResult,
) -> Result<NativeObservation, PublicationReceiptError> {
    let root_count: i64 = row.try_get("", "root_count")?;
    if !(0..=1).contains(&root_count) {
        return Err(integrity("native root is ambiguous"));
    }
    let root = if root_count == 1 {
        let root = NativeRoot {
            commit: row.try_get("", "root_commit")?,
            tree: row.try_get("", "root_tree")?,
        };
        validate_root(&root)?;
        Some(root)
    } else {
        None
    };
    let instance: Option<String> = row.try_get("", "instance_id")?;
    let Some(instance_id) = instance else {
        return Ok(NativeObservation { root, head: None });
    };
    if canonical_instance(&instance_id)? != instance_id {
        return Err(integrity("native head instance is not canonical"));
    }
    let head_root = NativeRoot {
        commit: row.try_get("", "head_commit")?,
        tree: row.try_get("", "head_tree")?,
    };
    validate_root(&head_root)?;
    if root.as_ref() != Some(&head_root) {
        return Err(integrity("native root bypassed its publication head"));
    }
    let token = NativePublicationToken {
        sequence: row.try_get("", "sequence")?,
        epoch: row.try_get("", "writer_epoch")?,
        certificate: row.try_get("", "certificate_receipt_id")?,
    };
    if token.sequence < 0 || token.epoch != NATIVE_EPOCH {
        return Err(PublicationReceiptError::Conflict(
            "native writer epoch is fenced".into(),
        ));
    }
    let state: String = row.try_get("", "state")?;
    let ready = match state.as_str() {
        "INITIALIZING" if token.certificate.is_none() => false,
        "READY" if token.certificate.is_some() => {
            let certificate_id: Option<i64> = row.try_get("", "certificate_id")?;
            let receipt_id: Option<i64> = row.try_get("", "receipt_id")?;
            let certificate_sequence: Option<i64> = row.try_get("", "certificate_sequence")?;
            let certificate_epoch: Option<i64> = row.try_get("", "certificate_epoch")?;
            let certificate_namespace: Option<String> = row.try_get("", "certificate_namespace")?;
            let certificate_instance: Option<String> = row.try_get("", "certificate_instance")?;
            let certificate_commit: Option<String> = row.try_get("", "certificate_commit")?;
            let certificate_tree: Option<String> = row.try_get("", "certificate_tree")?;
            let marker: Option<i32> = row.try_get("", "native_certificate_version")?;
            let version: Option<i32> = row.try_get("", "request_digest_version")?;
            let digest: Option<String> = row.try_get("", "request_digest")?;
            let writer_kind: Option<String> = row.try_get("", "writer_kind")?;
            let receipt_epoch: Option<i64> = row.try_get("", "receipt_epoch")?;
            let origin_path: Option<String> = row.try_get("", "origin_path")?;
            let origin_ref: Option<String> = row.try_get("", "origin_ref")?;
            let receipt_namespace: Option<String> = row.try_get("", "receipt_namespace")?;
            let old_oid: Option<String> = row.try_get("", "old_oid")?;
            let old_root: Option<String> = row.try_get("", "certificate_old_commit")?;
            let new_oid: Option<String> = row.try_get("", "new_oid")?;
            let path_commit: Option<String> = row.try_get("", "path_commit")?;
            let old_path: Option<String> = row.try_get("", "old_path_commit")?;
            let outbox: Option<i64> = row.try_get("", "outbox_id")?;
            let outbox_namespace: Option<String> = row.try_get("", "outbox_namespace")?;
            let outbox_sequence: Option<i64> = row.try_get("", "outbox_sequence")?;
            let receipt_sequence: Option<i64> = row.try_get("", "receipt_sequence")?;
            let digest_valid = digest.as_deref().is_some_and(|value| {
                value.len() == 71
                    && value.starts_with("sha256:")
                    && value[7..]
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
            if certificate_id != token.certificate
                || receipt_id != token.certificate
                || certificate_sequence != Some(token.sequence)
                || certificate_epoch != Some(token.epoch)
                || certificate_namespace.as_deref() != Some("/")
                || certificate_instance.as_deref() != Some(instance_id.as_str())
                || certificate_commit.as_deref() != Some(head_root.commit.as_str())
                || certificate_tree.as_deref() != Some(head_root.tree.as_str())
                || marker != Some(1)
                || version != Some(1)
                || !digest_valid
                || writer_kind.as_deref() != Some("trunk_push")
                || receipt_epoch != Some(token.epoch)
                || origin_ref.as_deref() != Some(MEGA_BRANCH_NAME)
                || origin_path.is_none()
                || origin_path != receipt_namespace
                || new_oid.is_none()
                || new_oid != path_commit
                || old_oid.is_none()
                || old_oid != old_root
                || old_path == path_commit
                || outbox.is_none()
                || outbox_namespace != receipt_namespace
                || outbox_sequence != receipt_sequence
            {
                return Err(integrity(
                    "native publication association is incomplete or inconsistent",
                ));
            }
            true
        }
        _ => return Err(integrity("invalid native head state")),
    };
    Ok(NativeObservation {
        root,
        head: Some(NativePublicationHead {
            root: head_root,
            instance_id,
            token,
            ready,
        }),
    })
}

async fn observe<C: ConnectionTrait>(
    connection: &C,
) -> Result<NativeObservation, PublicationReceiptError> {
    let rows = connection
        .query_all_raw(Statement::from_sql_and_values(
            connection.get_database_backend(),
            format!("WITH {NATIVE_OBSERVATION_CTES} SELECT * FROM native_observation"),
            [MEGA_BRANCH_NAME.into()],
        ))
        .await?;
    if rows.len() != 1 {
        return Err(integrity("ambiguous native publication observation"));
    }
    decode_native_observation(&rows[0])
}

async fn selected_ref<C: ConnectionTrait>(
    connection: &C,
    path: &str,
) -> Result<Option<NativeRoot>, PublicationReceiptError> {
    let refs = mega_refs::Entity::find()
        .filter(mega_refs::Column::Path.eq(path))
        .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
        .filter(mega_refs::Column::IsCl.eq(false))
        .all(connection)
        .await?;
    match refs.as_slice() {
        [] => Ok(None),
        [row] => {
            let root = NativeRoot {
                commit: row.ref_commit_hash.clone(),
                tree: row.ref_tree_hash.clone(),
            };
            validate_root(&root)?;
            Ok(Some(root))
        }
        _ => Err(integrity("selected native ref is ambiguous")),
    }
}

impl MonoStorage {
    pub(crate) async fn read_native_publication_head(
        &self,
        instance: &str,
    ) -> Result<NativePublicationHead, PublicationReceiptError> {
        let observation = observe(self.get_connection()).await?;
        let head = observation
            .head
            .ok_or_else(|| integrity("native publication is not initialized"))?;
        if !head.ready {
            return Err(integrity("native publication is not ready"));
        }
        if head.instance_id != canonical_instance(instance)? {
            return Err(integrity("native instance changed"));
        }
        Ok(head)
    }

    /// Maintenance-only, with stopped old writers and a drained queue; never invoked by resolve.
    pub(crate) async fn initialize_native_publication(
        &self,
        instance: &str,
    ) -> Result<(), PublicationReceiptError> {
        let instance = canonical_instance(instance)?;
        let txn = self.get_connection().begin().await?;
        PushQueueStorage::acquire_mono_write_lock(&txn)
            .await
            .map_err(|error| integrity(&error.to_string()))?;
        txn.execute_unprepared("SELECT id FROM queue_control WHERE id=1 FOR UPDATE")
            .await?;
        let observation = observe(&txn).await?;
        if observation.head.is_some() {
            return Err(integrity("native head already initialized"));
        }
        let root = observation
            .root
            .ok_or_else(|| integrity("native root missing"))?;
        let row = txn.query_one_raw(Statement::from_string(txn.get_database_backend(),
            "SELECT count(*)::bigint AS active FROM push_queue WHERE status IN ('Queued', 'Running')".to_owned())).await?
            .ok_or_else(|| integrity("queue observation missing"))?;
        if row.try_get::<i64>("", "active")? != 0 {
            return Err(integrity(
                "queue must be drained before native initialization",
            ));
        }
        let row = txn
            .query_one_raw(Statement::from_string(
                txn.get_database_backend(),
                r#"
          SELECT min(sequence) AS minimum, max(sequence) AS maximum FROM (
            SELECT sequence FROM mst2_namespace_seq UNION ALL SELECT sequence FROM mst2_publication
            UNION ALL SELECT observed_sequence AS sequence FROM mst2_queue_noop_receipt
          ) counters
        "#
                .to_owned(),
            ))
            .await?
            .ok_or_else(|| integrity("native sequence floor missing"))?;
        let minimum: Option<i64> = row.try_get("", "minimum")?;
        let floor = row.try_get::<Option<i64>>("", "maximum")?.unwrap_or(0);
        if minimum.is_some_and(|value| value < 0) || floor == i64::MAX {
            return Err(integrity("native sequence floor is invalid or exhausted"));
        }
        txn.execute_raw(Statement::from_sql_and_values(txn.get_database_backend(),
            "INSERT INTO mst2_native_head (namespace, instance_id, sequence, writer_epoch, root_commit, root_tree, state) \
             VALUES ('/', $1, $2, $3, $4, $5, 'INITIALIZING')",
            [instance.into(), floor.into(), NATIVE_EPOCH.into(), root.commit.into(), root.tree.into()],
        )).await?;
        txn.commit().await?;
        Ok(())
    }

    pub(crate) async fn reserve_native_publication_in_txn(
        &self,
        txn: &DatabaseTransaction,
        queue: &push_queue::Model,
        instance: &str,
    ) -> Result<PreparedNativePublication, PublicationReceiptError> {
        let owner = txn.query_one_raw(Statement::from_string(txn.get_database_backend(),
            "SELECT txid_current() AS owner FROM mst2_native_head WHERE namespace = '/' FOR UPDATE".to_owned()))
            .await?.ok_or_else(|| integrity("native publication is not initialized"))?;
        let observation = observe(txn).await?;
        let head = observation
            .head
            .ok_or_else(|| integrity("native publication is not initialized"))?;
        if head.instance_id != canonical_instance(instance)? {
            return Err(integrity("native instance changed"));
        }
        if queue.expected_native_sequence != Some(head.token.sequence)
            || queue.expected_native_epoch != Some(head.token.epoch)
            || queue.expected_native_certificate != head.token.certificate
            || queue.expected_commit_hash.as_deref() != Some(head.root.commit.as_str())
            || queue.expected_tree_hash.as_deref() != Some(head.root.tree.as_str())
        {
            return Err(PublicationReceiptError::Conflict(
                "claimed native publication token is stale or missing".into(),
            ));
        }
        let old_path = selected_ref(txn, &queue.path).await?;
        Ok(PreparedNativePublication {
            head,
            path: queue.path.clone(),
            old_path,
            transaction_id: owner.try_get("", "owner")?,
        })
    }

    async fn check_native_reservation(
        &self,
        txn: &DatabaseTransaction,
        prepared: &PreparedNativePublication,
    ) -> Result<(), PublicationReceiptError> {
        let row = txn.query_one_raw(Statement::from_sql_and_values(txn.get_database_backend(),
            "SELECT sequence FROM mst2_native_head WHERE namespace = '/' AND sequence = $1 AND writer_epoch = $2 \
             AND certificate_receipt_id IS NOT DISTINCT FROM $3 AND root_commit = $4 AND root_tree = $5 \
             AND txid_current() = $6 FOR UPDATE",
            [prepared.head.token.sequence.into(), prepared.head.token.epoch.into(), prepared.head.token.certificate.into(),
             prepared.head.root.commit.clone().into(), prepared.head.root.tree.clone().into(), prepared.transaction_id.into()],
        )).await?;
        if row.is_none() {
            return Err(PublicationReceiptError::Conflict(
                "native reservation is stale or belongs to another transaction".into(),
            ));
        }
        Ok(())
    }

    pub(crate) async fn finish_native_noop_in_txn(
        &self,
        txn: &DatabaseTransaction,
        prepared: PreparedNativePublication,
    ) -> Result<(), PublicationReceiptError> {
        self.check_native_reservation(txn, &prepared).await?;
        if selected_ref(txn, "/").await?.as_ref() != Some(&prepared.head.root)
            || selected_ref(txn, &prepared.path).await? != prepared.old_path
        {
            return Err(integrity("native no-op changed selected refs"));
        }
        Ok(())
    }

    pub(crate) async fn record_native_publication_in_txn(
        &self,
        txn: &DatabaseTransaction,
        prepared: PreparedNativePublication,
        committed: &CommittedPublication,
    ) -> Result<(), PublicationReceiptError> {
        self.check_native_reservation(txn, &prepared).await?;
        let root = selected_ref(txn, "/")
            .await?
            .ok_or_else(|| integrity("published native root missing"))?;
        let path = selected_ref(txn, &prepared.path)
            .await?
            .ok_or_else(|| integrity("published native path missing"))?;
        if prepared
            .old_path
            .as_ref()
            .is_some_and(|old| old.commit == path.commit)
            || committed.receipt.namespace != prepared.path
            || committed.receipt.new_oid != path.commit
            || committed.receipt.old_oid != prepared.head.root.commit
            || committed.receipt.writer_kind != "trunk_push"
            || committed.receipt.writer_epoch != NATIVE_EPOCH
            || committed.receipt.native_certificate_version.is_some()
            || committed.outbox.operation_id != committed.receipt.operation_id
            || committed.outbox.namespace != committed.receipt.namespace
            || committed.outbox.sequence != committed.receipt.sequence
        {
            return Err(integrity(
                "operation did not change its selected native ref",
            ));
        }
        let next = prepared
            .head
            .token
            .sequence
            .checked_add(1)
            .ok_or_else(|| integrity("native sequence exhausted"))?;
        let marked = txn.execute_raw(Statement::from_sql_and_values(txn.get_database_backend(),
            "UPDATE mst2_publication SET native_certificate_version = 1 WHERE id = $1 AND native_certificate_version IS NULL",
            [committed.receipt.id.into()],
        )).await?;
        if marked.rows_affected() != 1 {
            return Err(integrity("native receipt marker changed"));
        }
        mst2_native_publication::ActiveModel {
            receipt_id: Set(committed.receipt.id),
            namespace: Set("/".to_owned()),
            instance_id: Set(prepared.head.instance_id),
            sequence: Set(next),
            writer_epoch: Set(NATIVE_EPOCH),
            old_root_commit: Set(prepared.head.root.commit.clone()),
            old_root_tree: Set(prepared.head.root.tree.clone()),
            root_commit: Set(root.commit.clone()),
            root_tree: Set(root.tree.clone()),
            origin_path: Set(prepared.path),
            origin_ref: Set(MEGA_BRANCH_NAME.to_owned()),
            old_path_commit: Set(prepared.old_path.as_ref().map(|old| old.commit.clone())),
            old_path_tree: Set(prepared.old_path.map(|old| old.tree)),
            path_commit: Set(path.commit),
            path_tree: Set(path.tree),
        }
        .insert(txn)
        .await?;
        #[cfg(all(test, unix))]
        tests::crash_checkpoint("native-certificate-written");
        let updated = txn.execute_raw(Statement::from_sql_and_values(txn.get_database_backend(),
            "UPDATE mst2_native_head SET sequence = $1, root_commit = $2, root_tree = $3, state = 'READY', certificate_receipt_id = $4 \
             WHERE namespace = '/' AND sequence = $5 AND writer_epoch = $6 \
             AND certificate_receipt_id IS NOT DISTINCT FROM $7 AND root_commit = $8 AND root_tree = $9 AND txid_current() = $10",
            [next.into(), root.commit.into(), root.tree.into(), committed.receipt.id.into(), prepared.head.token.sequence.into(),
             NATIVE_EPOCH.into(), prepared.head.token.certificate.into(), prepared.head.root.commit.into(),
             prepared.head.root.tree.into(), prepared.transaction_id.into()],
        )).await?;
        if updated.rows_affected() != 1 {
            return Err(PublicationReceiptError::Conflict(
                "native head CAS failed".into(),
            ));
        }
        #[cfg(all(test, unix))]
        tests::crash_checkpoint("native-head-written");
        Ok(())
    }

    pub(crate) async fn validate_native_historical_receipt_in_txn(
        &self,
        txn: &DatabaseTransaction,
        receipt: &mst2_publication::Model,
    ) -> Result<(), PublicationReceiptError> {
        match receipt.native_certificate_version {
            None => return Ok(()),
            Some(1) => {}
            _ => return Err(integrity("unsupported native certificate version")),
        }
        let certificate = mst2_native_publication::Entity::find_by_id(receipt.id)
            .one(txn)
            .await?
            .ok_or_else(|| integrity("historical native certificate missing"))?;
        if certificate.namespace != "/"
            || certificate.sequence <= 0
            || certificate.writer_epoch != receipt.writer_epoch
            || certificate.origin_path != receipt.namespace
            || certificate.origin_ref != MEGA_BRANCH_NAME
            || receipt.writer_kind != "trunk_push"
            || certificate.old_root_commit != receipt.old_oid
            || certificate.path_commit != receipt.new_oid
            || certificate.old_path_commit.as_ref() == Some(&certificate.path_commit)
            || canonical_instance(&certificate.instance_id)? != certificate.instance_id
        {
            return Err(integrity("historical native certificate identity mismatch"));
        }
        validate_root(&NativeRoot {
            commit: certificate.root_commit,
            tree: certificate.root_tree,
        })?;
        validate_root(&NativeRoot {
            commit: certificate.old_root_commit,
            tree: certificate.old_root_tree,
        })?;
        validate_root(&NativeRoot {
            commit: certificate.path_commit,
            tree: certificate.path_tree,
        })?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "native_publication_tests.rs"]
mod tests;
