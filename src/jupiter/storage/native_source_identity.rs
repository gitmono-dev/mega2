//! Database-attested native identities, without policy or retention authority.

use git_internal::{
    hash::{HashKind, ObjectHash, get_hash_kind},
    internal::object::{ObjectTrait, commit::Commit, tree::Tree, types::ObjectType},
};
use mst2_codec::namespace::{NamespaceView, SourceSnapshot, radix};
use sea_orm::{
    AccessMode, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, DbErr, EntityTrait,
    IsolationLevel, QueryFilter, Statement, TransactionTrait, sea_query::Expr,
};

use super::{
    base_storage::StorageConnector, mono_storage::MonoStorage,
    mst2_publication_storage::PublicationReceiptError, push_queue_storage::PushQueueStorage,
};
use crate::callisto::{mega_commit, mega_tree};

const MAX_OBJECT_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMMIT_PARENTS: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeSourceIdentityError {
    #[error("native source identity requires PostgreSQL")]
    UnsupportedStorage,
    #[error("native namespace identity v1 requires SHA-1 objects")]
    UnsupportedObjectFormat,
    #[error("native source identity integrity: {0}")]
    Integrity(String),
    #[error(transparent)]
    Publication(#[from] PublicationReceiptError),
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error("native source bootstrap COMMIT outcome is unknown: {0}")]
    BootstrapUncertain(DbErr),
}

/// Constructed only after a single primary snapshot verifies actual objects.
/// These identities attest membership, never permission or continued retention.
#[derive(Debug, Clone)]
pub(crate) struct AttestedNativeIdentity {
    source: SourceSnapshot,
    namespace: NamespaceView,
    publication_sequence: i64,
    publication_epoch: i64,
    publication_certificate: i64,
}

impl AttestedNativeIdentity {
    pub(crate) fn source(&self) -> &SourceSnapshot {
        &self.source
    }

    pub(crate) fn namespace(&self) -> &NamespaceView {
        &self.namespace
    }

    pub(crate) fn publication_sequence(&self) -> i64 {
        self.publication_sequence
    }

    pub(crate) fn publication_epoch(&self) -> i64 {
        self.publication_epoch
    }

    pub(crate) fn publication_certificate(&self) -> i64 {
        self.publication_certificate
    }
}

impl MonoStorage {
    /// Explicit bootstrap. The source UUID is random and persisted once; it is
    /// neither a deployment UUID nor a hash of a path, object or configuration.
    /// A COMMIT error issues no successful identity; retry reads the durable row.
    pub(crate) async fn bootstrap_native_source_identity(
        &self,
        instance: &str,
    ) -> Result<String, NativeSourceIdentityError> {
        check_backend(self)?;
        let transaction = self.get_connection().begin().await?;
        require_primary(&transaction).await?;
        PushQueueStorage::acquire_mono_write_lock(&transaction)
            .await
            .map_err(|error| integrity(error.to_string()))?;
        self.read_native_publication_head_in_txn(&transaction, instance)
            .await?;
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO mst2_native_source_identity(singleton,source_id,scope_path)
             VALUES(1,$1::uuid,'/') ON CONFLICT(singleton) DO NOTHING",
                [uuid::Uuid::new_v4().to_string().into()],
            ))
            .await?;
        let source_id = read_source_id(&transaction).await?;
        transaction
            .commit()
            .await
            .map_err(NativeSourceIdentityError::BootstrapUncertain)?;
        Ok(source_id)
    }

    /// Capture a READY head and immutable source UUID in one repeatable primary
    /// snapshot. The caller supplies no source, commit, tree or digest witness.
    pub(crate) async fn attest_native_source_identity(
        &self,
        instance: &str,
    ) -> Result<AttestedNativeIdentity, NativeSourceIdentityError> {
        check_backend(self)?;
        let transaction = self
            .get_connection()
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await?;
        let identity = self
            .attest_native_source_in_snapshot(&transaction, instance)
            .await?;
        transaction.commit().await?;
        Ok(identity)
    }

    async fn attest_native_source_in_snapshot(
        &self,
        transaction: &DatabaseTransaction,
        instance: &str,
    ) -> Result<AttestedNativeIdentity, NativeSourceIdentityError> {
        require_primary(transaction).await?;
        let head = self
            .read_native_publication_head_in_txn(transaction, instance)
            .await?;
        let source_id = read_source_id(transaction).await?;
        let commit_id = sha1_oid(&head.root.commit)?;
        let tree_id = sha1_oid(&head.root.tree)?;
        // Bound database payloads before they are transferred to the decoder.
        let commit = mega_commit::Entity::find()
            .filter(mega_commit::Column::CommitId.eq(&head.root.commit))
            .filter(Expr::cust(format!(
                "CASE WHEN jsonb_typeof(parents_id::jsonb)='array' THEN
                 jsonb_array_length(parents_id::jsonb) <= {MAX_COMMIT_PARENTS} ELSE false END"
            )))
            .filter(Expr::cust(format!(
                "octet_length(COALESCE(author,'')) + octet_length(COALESCE(committer,'')) +
                 octet_length(COALESCE(content,'')) + octet_length(parents_id::text) <= {MAX_OBJECT_BYTES}"
            )))
            .one(transaction).await?
            .ok_or_else(|| integrity("captured native commit is missing or exceeds the validation budget"))?;
        let bytes = commit_bytes(&commit)?;
        let actual =
            ObjectHash::from_type_and_data_for_kind(HashKind::Sha1, ObjectType::Commit, &bytes)
                .map_err(|error| integrity(error.to_string()))?;
        if actual != commit_id {
            return Err(integrity(
                "persisted commit bytes do not match the captured native commit",
            ));
        }
        // commit_bytes is UTF-8, including its message; the parser's message
        // representation must never receive arbitrary non-UTF-8 database bytes.
        let parsed =
            Commit::from_bytes(&bytes, commit_id).map_err(|error| integrity(error.to_string()))?;
        if parsed.tree_id != tree_id {
            return Err(integrity(
                "captured native root tree is not the commit's root tree",
            ));
        }
        let tree = mega_tree::Entity::find()
            .filter(mega_tree::Column::TreeId.eq(&head.root.tree))
            .filter(Expr::cust(format!(
                "octet_length(sub_trees) <= {MAX_OBJECT_BYTES}"
            )))
            .one(transaction)
            .await?
            .ok_or_else(|| {
                integrity("captured native root tree is missing or exceeds the validation budget")
            })?;
        if tree.size < 0 || tree.size as usize != tree.sub_trees.len() {
            return Err(integrity("persisted native root tree size is inconsistent"));
        }
        let actual = ObjectHash::from_type_and_data_for_kind(
            HashKind::Sha1,
            ObjectType::Tree,
            &tree.sub_trees,
        )
        .map_err(|error| integrity(error.to_string()))?;
        if actual != tree_id {
            return Err(integrity(
                "persisted tree bytes do not match the captured native root tree",
            ));
        }
        // A matching typed hash does not make malformed bytes a Git tree.
        Tree::from_bytes(&tree.sub_trees, tree_id).map_err(|error| integrity(error.to_string()))?;
        let source = SourceSnapshot::new(source_id, "/".into(), head.root.commit, head.root.tree)
            .map_err(|error| integrity(error.to_string()))?;
        let namespace =
            NamespaceView::new(head.instance_id, source.clone(), radix::empty_root(), None)
                .map_err(|error| integrity(error.to_string()))?;
        let publication_certificate = head.token.certificate.ok_or_else(|| {
            integrity("READY native identity is missing its publication certificate")
        })?;
        Ok(AttestedNativeIdentity {
            source,
            namespace,
            publication_sequence: head.token.sequence,
            publication_epoch: head.token.epoch,
            publication_certificate,
        })
    }
}

fn check_backend(storage: &MonoStorage) -> Result<(), NativeSourceIdentityError> {
    if storage.get_connection().get_database_backend() != DbBackend::Postgres {
        return Err(NativeSourceIdentityError::UnsupportedStorage);
    }
    if get_hash_kind() != HashKind::Sha1 {
        return Err(NativeSourceIdentityError::UnsupportedObjectFormat);
    }
    Ok(())
}

async fn require_primary(
    transaction: &DatabaseTransaction,
) -> Result<(), NativeSourceIdentityError> {
    let row = transaction
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT pg_is_in_recovery() AS in_recovery",
        ))
        .await?
        .ok_or_else(|| integrity("native source primary status is missing"))?;
    if row.try_get::<bool>("", "in_recovery")? {
        return Err(integrity("native source identity requires the primary"));
    }
    Ok(())
}

async fn read_source_id(
    transaction: &DatabaseTransaction,
) -> Result<String, NativeSourceIdentityError> {
    let row = transaction
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT source_id::text,scope_path FROM mst2_native_source_identity WHERE singleton=1",
        ))
        .await?
        .ok_or_else(|| integrity("native source identity has not been bootstrapped"))?;
    let id: String = row.try_get("", "source_id")?;
    let scope: String = row.try_get("", "scope_path")?;
    let parsed = uuid::Uuid::parse_str(&id).map_err(|error| integrity(error.to_string()))?;
    if parsed.is_nil() || parsed.to_string() != id || scope != "/" {
        return Err(integrity("persisted native source binding is invalid"));
    }
    Ok(id)
}

fn sha1_oid(value: &str) -> Result<ObjectHash, NativeSourceIdentityError> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(NativeSourceIdentityError::UnsupportedObjectFormat);
    }
    ObjectHash::from_hex_for_kind(HashKind::Sha1, value)
        .map_err(|error| integrity(error.to_string()))
}

fn commit_bytes(model: &mega_commit::Model) -> Result<Vec<u8>, NativeSourceIdentityError> {
    sha1_oid(&model.commit_id)?;
    sha1_oid(&model.tree)?;
    let parents: Vec<String> = serde_json::from_value(model.parents_id.clone())
        .map_err(|error| integrity(error.to_string()))?;
    if parents.len() > MAX_COMMIT_PARENTS {
        return Err(integrity(
            "native commit exceeds the parent validation budget",
        ));
    }
    let author = model
        .author
        .as_deref()
        .ok_or_else(|| integrity("native commit author is missing"))?;
    let committer = model
        .committer
        .as_deref()
        .ok_or_else(|| integrity("native commit committer is missing"))?;
    let content = model
        .content
        .as_deref()
        .ok_or_else(|| integrity("native commit content is missing"))?;
    if !author.starts_with("author ")
        || !committer.starts_with("committer ")
        || author.contains(['\n', '\r'])
        || committer.contains(['\n', '\r'])
    {
        return Err(integrity("native commit has invalid signature headers"));
    }
    let mut bytes = format!("tree {}\n", model.tree).into_bytes();
    for parent in parents {
        sha1_oid(&parent)?;
        bytes.extend_from_slice(format!("parent {parent}\n").as_bytes());
    }
    bytes.extend_from_slice(author.as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(committer.as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(content.as_bytes());
    if bytes.len() > MAX_OBJECT_BYTES {
        return Err(integrity("native commit exceeds the validation budget"));
    }
    Ok(bytes)
}

fn integrity(message: impl Into<String>) -> NativeSourceIdentityError {
    NativeSourceIdentityError::Integrity(message.into())
}

#[cfg(test)]
#[path = "native_source_identity_tests.rs"]
mod tests;
