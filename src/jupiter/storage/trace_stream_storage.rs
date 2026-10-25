//! Append-only publication of `refs/libra/traces`.
//! One transaction publishes membership, the tip CAS, and the ref journal.

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    DatabaseTransaction, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, QuerySelect,
    Set, Statement, TransactionTrait, Value,
    sea_query::{Expr, OnConflict},
};
use uuid::Uuid;

use crate::{
    callisto::{trace_checkpoint, trace_object, trace_ref_update, trace_stream},
    ceres::traces::checkpoint_history::CheckpointValidatedPublication,
    common::{
        errors::{MegaError, TracesError},
        utils::is_protocol_zero_id,
    },
};

pub const TRACES_REF: &str = "refs/libra/traces";
const SCHEMA_MANIFEST_V1: &str = "manifest-v1";
const SCHEMA_METADATA_V2: &str = "metadata-v2";

/// How an archived schema is labeled. Unknown schemas are still published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaDisposition {
    Supported,
    Unsupported,
}

/// The result of one successful publication or an exact retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedRef {
    pub stream_id: String,
    pub new_tip: String,
    pub ref_sequence: i64,
    pub schema: SchemaDisposition,
    pub idempotent: bool,
}

/// One new commit and the parents the caller declares for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitParentList {
    pub oid: String,
    pub parents: Vec<String>,
}

/// A checkpoint that remains in the stream after this publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointBinding {
    pub checkpoint_key: String,
    pub original_tree: String,
}

/// Client-declared old tip condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedOldTip {
    /// No client condition.
    Unspecified,
    /// The ref must not exist yet.
    Absent,
    /// The ref must equal this tip.
    At(String),
}

/// The ref update checked inside the publication transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRef {
    pub stream_id: String,
    /// Client-declared old tip condition, checked under the stream lock.
    pub expected_old_tip: ExpectedOldTip,
    pub ref_names: Vec<String>,
    pub delete: bool,
    pub new_tip: String,
    pub commits: Vec<CommitParentList>,
    pub checkpoints: Vec<CheckpointBinding>,
    pub object_oids: Vec<String>,
    pub hash_kind: String,
    pub idempotency_key: String,
    pub schema_id: String,
}

/// Natural identity of one traces scope.
pub struct ScopeFacts<'a> {
    pub stream_id: &'a str,
    pub principal_id: &'a str,
    pub target_id: &'a str,
    pub source_repo_id: &'a str,
    pub epoch_id: &'a str,
    pub hash_kind: &'a str,
}

/// Publish the first ref of a scope, creating its stream row in the same
/// transaction when the natural tuple does not exist yet.
#[cfg(test)]
pub(in crate::jupiter) async fn publish_scope(
    db: &DatabaseConnection,
    facts: &ScopeFacts<'_>,
    update: &PublishRef,
    authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
) -> Result<PublishedRef, MegaError> {
    let txn = db.begin().await?;
    let published = match publish_scope_in_txn(&txn, facts, update, authority).await {
        Ok(published) => published,
        Err(err) => {
            txn.rollback().await?;
            return Err(err);
        }
    };
    txn.commit().await?;
    Ok(published)
}

/// First-scope publication inside a caller-owned transaction.
///
/// The stream is selected or inserted by its natural key before the ref
/// update, so the target binding, the stream identity and the tip publication
/// either all commit or all roll back.
#[cfg(test)]
pub(in crate::jupiter) async fn publish_scope_in_txn(
    txn: &DatabaseTransaction,
    facts: &ScopeFacts<'_>,
    update: &PublishRef,
    authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
) -> Result<PublishedRef, MegaError> {
    publish_scope_with_facts(txn, facts, update, authority, None).await
}

pub(in crate::jupiter) async fn publish_prepared_scope_in_txn(
    txn: &DatabaseTransaction,
    facts: &ScopeFacts<'_>,
    prepared: &CheckpointValidatedPublication,
    authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
) -> Result<PublishedRef, MegaError> {
    publish_scope_with_facts(txn, facts, prepared.update(), authority, Some(prepared)).await
}

async fn publish_scope_with_facts(
    txn: &DatabaseTransaction,
    facts: &ScopeFacts<'_>,
    update: &PublishRef,
    authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
    prepared: Option<&CheckpointValidatedPublication>,
) -> Result<PublishedRef, MegaError> {
    let hash_kind = canonical_hash_kind(facts.hash_kind)?;
    if hash_kind != canonical_hash_kind(&update.hash_kind)? {
        return Err(rejected());
    }
    let stream_id = canonical_uuid(facts.stream_id)?;
    if stream_id != facts.stream_id {
        return Err(rejected());
    }
    let stream_id = ensure_stream_in_txn(txn, facts, hash_kind).await?;
    let mut scoped = update.clone();
    scoped.stream_id = stream_id;
    publish_ref_in_txn(txn, &scoped, authority, prepared).await
}

/// Select the stream for the natural tuple or insert it. An existing row keeps
/// its identity; a concurrent insert resolves to the winner, and a different
/// `hash_kind` for the same tuple is rejected instead of reused.
async fn ensure_stream_in_txn(
    txn: &DatabaseTransaction,
    facts: &ScopeFacts<'_>,
    hash_kind: &str,
) -> Result<String, MegaError> {
    if let Some(row) = find_stream_by_natural_key(txn, facts).await? {
        if row.hash_kind != hash_kind || row.id != facts.stream_id {
            return Err(rejected());
        }
        return Ok(row.id);
    }
    let id = facts.stream_id.to_owned();
    let insert = trace_stream::Entity::insert(trace_stream::ActiveModel {
        id: Set(id.clone()),
        principal_id: Set(facts.principal_id.to_owned()),
        target_id: Set(facts.target_id.to_owned()),
        source_repo_id: Set(facts.source_repo_id.to_owned()),
        epoch_id: Set(facts.epoch_id.to_owned()),
        hash_kind: Set(hash_kind.to_owned()),
        tip_oid: Set(None),
        created_at: Set(Utc::now().fixed_offset()),
    })
    .on_conflict(
        OnConflict::columns([
            trace_stream::Column::PrincipalId,
            trace_stream::Column::TargetId,
            trace_stream::Column::SourceRepoId,
            trace_stream::Column::EpochId,
        ])
        .do_nothing()
        .to_owned(),
    )
    .exec(txn)
    .await;
    match insert {
        Ok(_) => Ok(id),
        Err(DbErr::RecordNotInserted) => {
            let row = find_stream_by_natural_key(txn, facts)
                .await?
                .ok_or_else(rejected)?;
            if row.hash_kind != hash_kind || row.id != facts.stream_id {
                return Err(rejected());
            }
            Ok(row.id)
        }
        Err(error) => Err(error.into()),
    }
}

async fn find_stream_by_natural_key(
    txn: &DatabaseTransaction,
    facts: &ScopeFacts<'_>,
) -> Result<Option<trace_stream::Model>, MegaError> {
    Ok(trace_stream::Entity::find()
        .filter(trace_stream::Column::PrincipalId.eq(facts.principal_id))
        .filter(trace_stream::Column::TargetId.eq(facts.target_id))
        .filter(trace_stream::Column::SourceRepoId.eq(facts.source_repo_id))
        .filter(trace_stream::Column::EpochId.eq(facts.epoch_id))
        .one(txn)
        .await?)
}

fn canonical_hash_kind(value: &str) -> Result<&str, MegaError> {
    match value {
        "sha1" => Ok("sha1"),
        "sha256" => Ok("sha256"),
        _ => Err(rejected()),
    }
}

/// Publish one fast-forward ref update. A rejection writes nothing.
#[cfg(test)]
pub(in crate::jupiter) async fn publish_ref(
    db: &DatabaseConnection,
    update: &PublishRef,
    authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
) -> Result<PublishedRef, MegaError> {
    let txn = db.begin().await?;
    let published = match publish_ref_in_txn(&txn, update, authority, None).await {
        Ok(published) => published,
        Err(err) => {
            txn.rollback().await?;
            return Err(err);
        }
    };
    txn.commit().await?;
    Ok(published)
}

pub(in crate::jupiter) async fn publish_prepared_ref(
    db: &DatabaseConnection,
    prepared: &CheckpointValidatedPublication,
    authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
) -> Result<PublishedRef, MegaError> {
    let control = prepared.control();
    let txn = control.run(async { Ok(db.begin().await?) }).await?;
    let result = control
        .run(publish_ref_in_txn(
            &txn,
            prepared.update(),
            authority,
            Some(prepared),
        ))
        .await;
    match result {
        Ok(published) => {
            control.check()?;
            txn.commit().await?;
            Ok(published)
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

async fn publish_ref_in_txn(
    txn: &DatabaseTransaction,
    update: &PublishRef,
    authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
    prepared: Option<&CheckpointValidatedPublication>,
) -> Result<PublishedRef, MegaError> {
    let PreparedRef {
        stream_id,
        expected_old_tip,
        new_tip,
        commits,
        checkpoints,
        object_oids,
        hash_kind,
        schema,
    } = prepare_ref(update)?;

    let (stored_hash_kind, old_tip) = lock_stream(txn, &stream_id).await?;
    if stored_hash_kind != hash_kind {
        return Err(rejected());
    }
    crate::ceres::traces::auth::recheck_publish_authority(txn, &stream_id, authority).await?;
    if let Some(existing) = journal_for_key(txn, &update.idempotency_key).await? {
        if existing.stream_id == stream_id
            && existing.new_tip == new_tip
            && old_tip.as_deref() == Some(new_tip.as_str())
        {
            return Ok(PublishedRef {
                stream_id,
                new_tip,
                ref_sequence: existing.ref_sequence,
                schema,
                idempotent: true,
            });
        }
        return Err(rejected());
    }
    // The expected-old condition is checked only for a new publication: an
    // exact retry of an already successful request still returns its journal.
    match &expected_old_tip {
        ExpectedOldTip::Unspecified => {}
        ExpectedOldTip::Absent if old_tip.is_some() => return Err(rejected()),
        ExpectedOldTip::At(expected) if old_tip.as_deref() != Some(expected.as_str()) => {
            return Err(rejected());
        }
        _ => {}
    }
    require_fast_forward(old_tip.as_deref(), &new_tip, &commits)?;
    match prepared {
        Some(prepared) => publish_prepared_checkpoints(txn, &stream_id, prepared).await?,
        None => publish_checkpoints(txn, &stream_id, &checkpoints).await?,
    }
    publish_membership(txn, &stream_id, &hash_kind, &object_oids).await?;
    cas_tip(txn, &stream_id, old_tip.as_deref(), &new_tip).await?;
    let ref_sequence = append_journal(
        txn,
        &stream_id,
        old_tip.as_deref(),
        &new_tip,
        &update.idempotency_key,
    )
    .await?;
    Ok(PublishedRef {
        stream_id,
        new_tip,
        ref_sequence,
        schema,
        idempotent: false,
    })
}

struct PreparedRef {
    stream_id: String,
    expected_old_tip: ExpectedOldTip,
    new_tip: String,
    commits: Vec<CommitParentList>,
    checkpoints: Vec<CheckpointBinding>,
    object_oids: Vec<String>,
    hash_kind: String,
    schema: SchemaDisposition,
}

/// Reject a ref update that cannot be published, before any bytes or rows change.
pub fn preflight(update: &PublishRef) -> Result<(), MegaError> {
    prepare_ref(update).map(|_| ())
}

fn prepare_ref(update: &PublishRef) -> Result<PreparedRef, MegaError> {
    let stream_id = canonical_uuid(&update.stream_id)?;
    require_traces_ref(&update.ref_names)?;
    if update.delete || update.new_tip.is_empty() || is_protocol_zero_id(&update.new_tip) {
        return Err(rejected());
    }
    if update.idempotency_key.is_empty() {
        return Err(rejected());
    }
    let hash_kind = match update.hash_kind.as_str() {
        "sha1" => "sha1",
        "sha256" => "sha256",
        _ => return Err(rejected()),
    };
    let new_tip = canonical_oid(&update.new_tip)?;
    let expected_old_tip = match &update.expected_old_tip {
        ExpectedOldTip::Unspecified => ExpectedOldTip::Unspecified,
        ExpectedOldTip::Absent => ExpectedOldTip::Absent,
        ExpectedOldTip::At(value) => ExpectedOldTip::At(canonical_oid(value)?),
    };
    let commits = canonical_commits(&update.commits)?;
    let checkpoints = canonical_checkpoints(&update.checkpoints)?;
    let object_oids = canonical_oids(&update.object_oids)?;
    require_single_parents(&commits)?;
    require_membership(&new_tip, &commits, &object_oids)?;
    Ok(PreparedRef {
        stream_id,
        expected_old_tip,
        new_tip,
        commits,
        checkpoints,
        object_oids,
        hash_kind: hash_kind.to_owned(),
        schema: schema_disposition(&update.schema_id),
    })
}

fn schema_disposition(schema_id: &str) -> SchemaDisposition {
    if schema_id == SCHEMA_MANIFEST_V1 || schema_id == SCHEMA_METADATA_V2 {
        SchemaDisposition::Supported
    } else {
        SchemaDisposition::Unsupported
    }
}

fn require_traces_ref(ref_names: &[String]) -> Result<(), MegaError> {
    if ref_names.len() == 1 && ref_names[0] == TRACES_REF {
        Ok(())
    } else {
        Err(rejected())
    }
}

fn require_single_parents(commits: &[CommitParentList]) -> Result<(), MegaError> {
    if commits.iter().all(|commit| commit.parents.len() <= 1) {
        Ok(())
    } else {
        Err(rejected())
    }
}

fn require_membership(
    new_tip: &str,
    commits: &[CommitParentList],
    object_oids: &[String],
) -> Result<(), MegaError> {
    let listed = |oid: &str| object_oids.iter().any(|listed| listed == oid);
    if listed(new_tip) && commits.iter().all(|commit| listed(&commit.oid)) {
        Ok(())
    } else {
        Err(rejected())
    }
}

fn require_fast_forward(
    old_tip: Option<&str>,
    new_tip: &str,
    commits: &[CommitParentList],
) -> Result<(), MegaError> {
    let mut expect_parent = old_tip.map(str::to_owned);
    if commits.is_empty() {
        return Err(rejected());
    }
    for commit in commits {
        match (&expect_parent, commit.parents.as_slice()) {
            (None, []) => {}
            (Some(parent), [only]) if only == parent => {}
            _ => return Err(rejected()),
        }
        expect_parent = Some(commit.oid.clone());
    }
    if expect_parent.as_deref() == Some(new_tip) {
        Ok(())
    } else {
        Err(rejected())
    }
}

async fn lock_stream(
    txn: &DatabaseTransaction,
    stream_id: &str,
) -> Result<(String, Option<String>), MegaError> {
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT hash_kind, tip_oid FROM trace_stream WHERE id = $1 FOR UPDATE",
            [Value::from(stream_id.to_owned())],
        ))
        .await?
        .ok_or_else(rejected)?;
    let hash_kind: String = row.try_get("", "hash_kind")?;
    let tip_oid: Option<String> = row.try_get("", "tip_oid")?;
    Ok((hash_kind, tip_oid))
}

async fn journal_for_key(
    txn: &DatabaseTransaction,
    idempotency_key: &str,
) -> Result<Option<trace_ref_update::Model>, MegaError> {
    Ok(trace_ref_update::Entity::find()
        .filter(trace_ref_update::Column::IdempotencyKey.eq(idempotency_key))
        .one(txn)
        .await?)
}

async fn publish_prepared_checkpoints(
    txn: &DatabaseTransaction,
    stream_id: &str,
    prepared: &CheckpointValidatedPublication,
) -> Result<(), MegaError> {
    let proposed = prepared.final_checkpoints()?;
    let mut after = None::<String>;
    loop {
        prepared.control().check()?;
        let mut query = trace_checkpoint::Entity::find()
            .filter(trace_checkpoint::Column::StreamId.eq(stream_id));
        if let Some(after) = &after {
            query = query.filter(trace_checkpoint::Column::Id.gt(after.clone()));
        }
        let rows = query
            .order_by_asc(trace_checkpoint::Column::Id)
            .limit(128)
            .all(txn)
            .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            prepared.control().checkpoint().await?;
            if proposed
                .get(&row.checkpoint_key)?
                .is_none_or(|binding| binding.original_tree != row.original_tree)
            {
                return Err(rejected());
            }
            after = Some(row.id);
        }
    }
    for checkpoint in proposed.iter()? {
        prepared.control().checkpoint().await?;
        let checkpoint = checkpoint?;
        let existing = trace_checkpoint::Entity::find()
            .filter(trace_checkpoint::Column::StreamId.eq(stream_id))
            .filter(trace_checkpoint::Column::CheckpointKey.eq(&checkpoint.checkpoint_key))
            .one(txn)
            .await?;
        if existing.is_some() {
            continue;
        }
        trace_checkpoint::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.to_owned()),
            checkpoint_key: Set(checkpoint.checkpoint_key),
            original_tree: Set(checkpoint.original_tree),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

async fn publish_checkpoints(
    txn: &DatabaseTransaction,
    stream_id: &str,
    proposed: &[CheckpointBinding],
) -> Result<(), MegaError> {
    let existing = trace_checkpoint::Entity::find()
        .filter(trace_checkpoint::Column::StreamId.eq(stream_id))
        .all(txn)
        .await?;
    for row in &existing {
        let kept = proposed
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_key == row.checkpoint_key);
        if kept.is_none_or(|checkpoint| checkpoint.original_tree != row.original_tree) {
            return Err(rejected());
        }
    }
    for checkpoint in proposed {
        if existing
            .iter()
            .any(|row| row.checkpoint_key == checkpoint.checkpoint_key)
        {
            continue;
        }
        trace_checkpoint::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.to_owned()),
            checkpoint_key: Set(checkpoint.checkpoint_key.clone()),
            original_tree: Set(checkpoint.original_tree.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

async fn publish_membership(
    txn: &DatabaseTransaction,
    stream_id: &str,
    hash_kind: &str,
    object_oids: &[String],
) -> Result<(), MegaError> {
    for oid in object_oids {
        let existing = trace_object::Entity::find()
            .filter(trace_object::Column::StreamId.eq(stream_id))
            .filter(trace_object::Column::HashKind.eq(hash_kind))
            .filter(trace_object::Column::Oid.eq(oid))
            .one(txn)
            .await?;
        if let Some(existing) = existing {
            if !existing.visible {
                let mut active = existing.into_active_model();
                active.visible = Set(true);
                active.update(txn).await?;
            }
            continue;
        }
        trace_object::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.to_owned()),
            hash_kind: Set(hash_kind.to_owned()),
            oid: Set(oid.clone()),
            visible: Set(true),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

async fn cas_tip(
    txn: &DatabaseTransaction,
    stream_id: &str,
    old_tip: Option<&str>,
    new_tip: &str,
) -> Result<(), MegaError> {
    let mut update = trace_stream::Entity::update_many()
        .col_expr(
            trace_stream::Column::TipOid,
            Expr::value(new_tip.to_owned()),
        )
        .filter(trace_stream::Column::Id.eq(stream_id));
    update = match old_tip {
        Some(old_tip) => update.filter(trace_stream::Column::TipOid.eq(old_tip)),
        None => update.filter(trace_stream::Column::TipOid.is_null()),
    };
    let result = update.exec(txn).await?;
    if result.rows_affected == 1 {
        Ok(())
    } else {
        Err(rejected())
    }
}

async fn append_journal(
    txn: &DatabaseTransaction,
    stream_id: &str,
    old_tip: Option<&str>,
    new_tip: &str,
    idempotency_key: &str,
) -> Result<i64, MegaError> {
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(MAX(ref_sequence), 0)::bigint AS n \
             FROM trace_ref_update WHERE stream_id = $1",
            [Value::from(stream_id.to_owned())],
        ))
        .await?
        .ok_or_else(rejected)?;
    let current: i64 = row.try_get("", "n")?;
    let ref_sequence = current + 1;
    trace_ref_update::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.to_owned()),
        ref_sequence: Set(ref_sequence),
        idempotency_key: Set(idempotency_key.to_owned()),
        old_tip: Set(old_tip.map(str::to_owned)),
        new_tip: Set(new_tip.to_owned()),
        state: Set("pending".to_owned()),
        claim_until: Set(None),
        attempt_count: Set(0),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(txn)
    .await?;
    Ok(ref_sequence)
}

fn canonical_uuid(value: &str) -> Result<String, MegaError> {
    Uuid::parse_str(value)
        .map(|id| id.to_string())
        .map_err(|_| rejected())
}

fn canonical_oid(oid: &str) -> Result<String, MegaError> {
    if oid.is_empty() || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(rejected());
    }
    Ok(oid.to_ascii_lowercase())
}

fn canonical_oids(oids: &[String]) -> Result<Vec<String>, MegaError> {
    oids.iter().map(|oid| canonical_oid(oid)).collect()
}

fn canonical_commits(commits: &[CommitParentList]) -> Result<Vec<CommitParentList>, MegaError> {
    commits
        .iter()
        .map(|commit| {
            Ok(CommitParentList {
                oid: canonical_oid(&commit.oid)?,
                parents: canonical_oids(&commit.parents)?,
            })
        })
        .collect()
}

fn canonical_checkpoints(
    checkpoints: &[CheckpointBinding],
) -> Result<Vec<CheckpointBinding>, MegaError> {
    checkpoints
        .iter()
        .map(|checkpoint| {
            if checkpoint.checkpoint_key.is_empty() {
                return Err(rejected());
            }
            Ok(CheckpointBinding {
                checkpoint_key: checkpoint.checkpoint_key.clone(),
                original_tree: canonical_oid(&checkpoint.original_tree)?,
            })
        })
        .collect()
}

fn rejected() -> MegaError {
    MegaError::Traces(TracesError::InvalidParameter)
}

#[cfg(test)]
mod tests {
    use sea_orm::{EntityTrait, PaginatorTrait, Set};
    use uuid::Uuid;

    use super::*;
    use crate::{
        callisto::{
            trace_checkpoint, trace_object, trace_principal, trace_ref_update, trace_target,
        },
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const OID_C: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const TREE_A: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const TREE_B: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    async fn open_db() -> (tempfile::TempDir, DatabaseConnection) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        (dir, db)
    }

    #[derive(Clone)]
    struct Scope {
        stream_id: String,
        principal_id: String,
        target_id: String,
        path: String,
    }

    async fn stream(db: &DatabaseConnection) -> Scope {
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let path = format!("/team/{target_id}");
        let now = Utc::now().fixed_offset();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("principal");
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(path.clone()),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("target");
        crate::callisto::trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            namespace: Set("static_push_token".to_owned()),
            credential_id: Set("publish-operator".to_owned()),
            created_at: Set(now),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .expect("binding");
        let stream_id = trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            target_id: Set(target_id.clone()),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("stream")
        .id;
        Scope {
            stream_id,
            principal_id,
            target_id,
            path,
        }
    }

    async fn scope_target(db: &DatabaseConnection) -> Scope {
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let path = format!("/team/{target_id}");
        let now = Utc::now().fixed_offset();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("principal");
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(path.clone()),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("target");
        crate::callisto::trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            namespace: Set("static_push_token".to_owned()),
            credential_id: Set("publish-operator".to_owned()),
            created_at: Set(now),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .expect("binding");
        let stream_id = Uuid::new_v4().to_string();
        Scope {
            stream_id,
            principal_id,
            target_id,
            path,
        }
    }

    async fn stream_rows(db: &DatabaseConnection, scope: &Scope) -> u64 {
        trace_stream::Entity::find()
            .filter(trace_stream::Column::TargetId.eq(&scope.target_id))
            .filter(trace_stream::Column::PrincipalId.eq(&scope.principal_id))
            .count(db)
            .await
            .expect("stream rows")
    }

    fn authority<'a>(
        scope: &'a Scope,
        token: &'a crate::config::PushTokenConfig,
    ) -> crate::ceres::traces::auth::PublishAuthority<'a> {
        use crate::ceres::traces::auth::{PublishAuthority, TracesCaller};
        PublishAuthority {
            caller: TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token,
            },
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            authorized_path: &scope.path,
            expected_generation: 1,
            policy: None,
        }
    }

    fn token(path: &str) -> crate::config::PushTokenConfig {
        crate::config::PushTokenConfig {
            name: "publish-operator".to_owned(),
            token: "synthetic-secret".to_owned(),
            paths: Some(vec![path.to_owned()]),
        }
    }

    async fn publish(
        db: &DatabaseConnection,
        scope: &Scope,
        update: &PublishRef,
    ) -> Result<PublishedRef, MegaError> {
        let token = token(&scope.path);
        publish_ref(db, update, &authority(scope, &token)).await
    }

    fn update(stream_id: &str, new_tip: &str, parents: Vec<String>, key: &str) -> PublishRef {
        PublishRef {
            stream_id: stream_id.to_owned(),
            expected_old_tip: ExpectedOldTip::Unspecified,
            ref_names: vec![TRACES_REF.to_owned()],
            delete: false,
            new_tip: new_tip.to_owned(),
            commits: vec![CommitParentList {
                oid: new_tip.to_owned(),
                parents,
            }],
            checkpoints: Vec::new(),
            object_oids: vec![new_tip.to_owned()],
            hash_kind: "sha1".to_owned(),
            idempotency_key: key.to_owned(),
            schema_id: SCHEMA_MANIFEST_V1.to_owned(),
        }
    }

    async fn tip(db: &DatabaseConnection, stream_id: &str) -> Option<String> {
        trace_stream::Entity::find_by_id(stream_id)
            .one(db)
            .await
            .expect("stream")
            .expect("row")
            .tip_oid
    }

    async fn visible_count(db: &DatabaseConnection, stream_id: &str) -> u64 {
        trace_object::Entity::find()
            .filter(trace_object::Column::StreamId.eq(stream_id))
            .filter(trace_object::Column::Visible.eq(true))
            .count(db)
            .await
            .expect("visible")
    }

    async fn journal_count(db: &DatabaseConnection, stream_id: &str) -> u64 {
        trace_ref_update::Entity::find()
            .filter(trace_ref_update::Column::StreamId.eq(stream_id))
            .count(db)
            .await
            .expect("journal")
    }

    #[tokio::test]
    async fn ref_transaction_accepts_only_traces_ref_and_rejects_delete() {
        let (_dir, db) = open_db().await;
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        let mut extra = update(&stream_id, OID_A, Vec::new(), "extra-ref");
        extra.ref_names = vec!["refs/heads/main".to_owned()];
        assert!(publish(&db, &scope, &extra).await.is_err());
        let mut both = update(&stream_id, OID_A, Vec::new(), "both-refs");
        both.ref_names = vec![TRACES_REF.to_owned(), "refs/heads/main".to_owned()];
        assert!(publish(&db, &scope, &both).await.is_err());
        let mut deletion = update(&stream_id, OID_A, Vec::new(), "delete-ref");
        deletion.delete = true;
        assert!(publish(&db, &scope, &deletion).await.is_err());
        let mut zero = update(&stream_id, &"0".repeat(40), Vec::new(), "zero-ref");
        zero.object_oids = vec!["0".repeat(40)];
        zero.commits[0].oid = "0".repeat(40);
        assert!(publish(&db, &scope, &zero).await.is_err());
        assert_eq!(tip(&db, &stream_id).await, None);
        assert_eq!(visible_count(&db, &stream_id).await, 0);
        assert_eq!(journal_count(&db, &stream_id).await, 0);
    }

    #[tokio::test]
    async fn ref_transaction_rejects_non_ff_and_multi_parent() {
        let (_dir, db) = open_db().await;
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        publish(&db, &scope, &update(&stream_id, OID_A, Vec::new(), "first"))
            .await
            .expect("first");
        let non_ff = update(&stream_id, OID_C, Vec::new(), "orphan");
        assert!(publish(&db, &scope, &non_ff).await.is_err());
        let mut merge = update(
            &stream_id,
            OID_B,
            vec![OID_A.to_owned(), OID_C.to_owned()],
            "merge",
        );
        merge.object_oids = vec![OID_B.to_owned()];
        assert!(publish(&db, &scope, &merge).await.is_err());
        assert_eq!(tip(&db, &stream_id).await.as_deref(), Some(OID_A));
        assert_eq!(visible_count(&db, &stream_id).await, 1);
        assert_eq!(journal_count(&db, &stream_id).await, 1);
    }

    #[tokio::test]
    async fn ref_transaction_keeps_checkpoint_tree_and_rejects_delete() {
        let (_dir, db) = open_db().await;
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        let mut first = update(&stream_id, OID_A, Vec::new(), "ckpt-first");
        first.checkpoints = vec![CheckpointBinding {
            checkpoint_key: "ckpt-1".to_owned(),
            original_tree: TREE_A.to_owned(),
        }];
        publish(&db, &scope, &first).await.expect("first");
        let mut changed = update(&stream_id, OID_B, vec![OID_A.to_owned()], "ckpt-change");
        changed.checkpoints = vec![CheckpointBinding {
            checkpoint_key: "ckpt-1".to_owned(),
            original_tree: TREE_B.to_owned(),
        }];
        assert!(publish(&db, &scope, &changed).await.is_err());
        let removed = update(&stream_id, OID_B, vec![OID_A.to_owned()], "ckpt-remove");
        assert!(publish(&db, &scope, &removed).await.is_err());
        let mut kept = update(&stream_id, OID_B, vec![OID_A.to_owned()], "ckpt-keep");
        kept.checkpoints = vec![
            CheckpointBinding {
                checkpoint_key: "ckpt-1".to_owned(),
                original_tree: TREE_A.to_owned(),
            },
            CheckpointBinding {
                checkpoint_key: "ckpt-2".to_owned(),
                original_tree: TREE_B.to_owned(),
            },
        ];
        publish(&db, &scope, &kept).await.expect("kept");
        let rows = trace_checkpoint::Entity::find()
            .filter(trace_checkpoint::Column::StreamId.eq(&stream_id))
            .all(&db)
            .await
            .expect("checkpoints");
        let tree = rows
            .iter()
            .find(|row| row.checkpoint_key == "ckpt-1")
            .expect("ckpt-1")
            .original_tree
            .clone();
        assert_eq!(tree, TREE_A);
        assert_eq!(rows.len(), 2);
        assert_eq!(tip(&db, &stream_id).await.as_deref(), Some(OID_B));
    }

    #[tokio::test]
    async fn ref_transaction_commits_membership_ref_and_journal_together() {
        let (_dir, db) = open_db().await;
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        trace_ref_update::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            ref_sequence: Set(1),
            idempotency_key: Set("occupied-key".to_owned()),
            old_tip: Set(None),
            new_tip: Set(OID_A.to_owned()),
            state: Set("pending".to_owned()),
            claim_until: Set(None),
            attempt_count: Set(0),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("occupied journal");
        let blocked = update(&stream_id, OID_A, Vec::new(), "occupied-key");
        assert!(publish(&db, &scope, &blocked).await.is_err());
        assert_eq!(tip(&db, &stream_id).await, None);
        assert_eq!(visible_count(&db, &stream_id).await, 0);
        assert_eq!(journal_count(&db, &stream_id).await, 1);

        let published = publish(
            &db,
            &scope,
            &update(&stream_id, OID_A, Vec::new(), "fresh-key"),
        )
        .await
        .expect("publish");
        assert!(!published.idempotent);
        assert_eq!(published.schema, SchemaDisposition::Supported);
        assert_eq!(tip(&db, &stream_id).await.as_deref(), Some(OID_A));
        assert_eq!(visible_count(&db, &stream_id).await, 1);
        assert_eq!(journal_count(&db, &stream_id).await, 2);
        let replay = publish(
            &db,
            &scope,
            &update(&stream_id, OID_A, Vec::new(), "fresh-key"),
        )
        .await
        .expect("replay");
        assert!(replay.idempotent);
        assert_eq!(journal_count(&db, &stream_id).await, 2);

        let unknown = {
            let mut update = update(&stream_id, OID_B, vec![OID_A.to_owned()], "unknown-schema");
            update.schema_id = "lifecycle-unknown".to_owned();
            update
        };
        let archived = publish(&db, &scope, &unknown)
            .await
            .expect("unknown schema");
        assert_eq!(archived.schema, SchemaDisposition::Unsupported);
        assert_eq!(tip(&db, &stream_id).await.as_deref(), Some(OID_B));
        assert_eq!(journal_count(&db, &stream_id).await, 3);
    }

    #[tokio::test]
    async fn ref_transaction_concurrent_old_tip_has_one_winner() {
        let (_dir, db) = open_db().await;
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        let left_db = db.clone();
        let right_db = db.clone();
        let left_scope = scope.clone();
        let right_scope = scope.clone();
        let left = tokio::spawn(async move {
            let stream_id = left_scope.stream_id.clone();
            publish(
                &left_db,
                &left_scope,
                &update(&stream_id, OID_A, Vec::new(), "race-left"),
            )
            .await
        });
        let right = tokio::spawn(async move {
            let stream_id = right_scope.stream_id.clone();
            publish(
                &right_db,
                &right_scope,
                &update(&stream_id, OID_B, Vec::new(), "race-right"),
            )
            .await
        });
        let left = left.await.expect("left");
        let right = right.await.expect("right");
        let stored = tip(&db, &stream_id).await.expect("tip");
        match (left, right) {
            (Ok(published), Err(_)) => assert_eq!(published.new_tip, stored),
            (Err(_), Ok(published)) => assert_eq!(published.new_tip, stored),
            (Ok(_), Ok(_)) => panic!("both publications succeeded"),
            (Err(left_err), Err(right_err)) => {
                panic!("both publications failed: {left_err}; {right_err}")
            }
        }
        assert!(stored == OID_A || stored == OID_B);
        assert_eq!(journal_count(&db, &stream_id).await, 1);
        assert_eq!(visible_count(&db, &stream_id).await, 1);
    }

    #[tokio::test]
    async fn publish_generation_rejects_a_stale_generation() {
        let (_dir, db) = open_db().await;
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        publish(
            &db,
            &scope,
            &update(&stream_id, OID_A, Vec::new(), "gen-first"),
        )
        .await
        .expect("generation 1");
        crate::callisto::audit_logs::ActiveModel {
            id: Set(910_037),
            actor_id: Set(0),
            actor_type: Set(crate::callisto::sea_orm_active_enums::ActorTypeEnum::Human),
            action: Set(crate::callisto::sea_orm_active_enums::AuditActionEnum::Update),
            target_type: Set(crate::callisto::sea_orm_active_enums::TargetTypeEnum::Repository),
            target_id: Set(0),
            metadata: Set(Some(serde_json::json!({
                "trace_target_id": scope.target_id,
                "binding_generation": 2
            }))),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("generation audit");
        let stale = update(&stream_id, OID_B, vec![OID_A.to_owned()], "gen-stale");
        assert!(publish(&db, &scope, &stale).await.is_err());
        assert_eq!(tip(&db, &stream_id).await.as_deref(), Some(OID_A));
        assert_eq!(journal_count(&db, &stream_id).await, 1);
        let token = token(&scope.path);
        let mut current = authority(&scope, &token);
        current.expected_generation = 2;
        publish_ref(
            &db,
            &update(&stream_id, OID_B, vec![OID_A.to_owned()], "gen-current"),
            &current,
        )
        .await
        .expect("generation 2");
        assert_eq!(tip(&db, &stream_id).await.as_deref(), Some(OID_B));
    }
    #[tokio::test]
    async fn first_scope_atomic_creates_one_stream_and_keeps_identity() {
        let (_dir, db) = open_db().await;
        let scope = scope_target(&db).await;
        let epoch = Uuid::new_v4().to_string();
        let facts = ScopeFacts {
            stream_id: &scope.stream_id,
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            source_repo_id: "source-opaque",
            epoch_id: &epoch,
            hash_kind: "sha1",
        };
        let token = token(&scope.path);
        let auth = authority(&scope, &token);

        let first = publish_scope(
            &db,
            &facts,
            &update("", OID_A, Vec::new(), "first-scope"),
            &auth,
        )
        .await
        .expect("first scope publish");
        assert_eq!(stream_rows(&db, &scope).await, 1);
        assert_eq!(tip(&db, &first.stream_id).await.as_deref(), Some(OID_A));
        assert_eq!(journal_count(&db, &first.stream_id).await, 1);

        // AC-8: the exact retry is idempotent and does not add a journal row.
        let replay = publish_scope(
            &db,
            &facts,
            &update("", OID_A, Vec::new(), "first-scope"),
            &auth,
        )
        .await
        .expect("idempotent retry");
        assert!(replay.idempotent);
        assert_eq!(replay.stream_id, first.stream_id);
        assert_eq!(journal_count(&db, &first.stream_id).await, 1);
        assert_eq!(stream_rows(&db, &scope).await, 1);

        // AC-5: a client-declared old tip that disagrees with the locked tip
        // rejects before any write, even when the update is otherwise a fast-forward.
        let mut wrong_old = update("", OID_B, vec![OID_A.to_owned()], "wrong-old");
        wrong_old.expected_old_tip = ExpectedOldTip::At(OID_C.to_owned());
        assert!(publish_scope(&db, &facts, &wrong_old, &auth).await.is_err());
        assert_eq!(tip(&db, &first.stream_id).await.as_deref(), Some(OID_A));

        // AC-4: the existing scope keeps its identity on later updates, and the
        // matching declared old tip is accepted.
        let mut second = update("", OID_B, vec![OID_A.to_owned()], "second-scope");
        second.expected_old_tip = ExpectedOldTip::At(OID_A.to_owned());
        second.checkpoints = vec![CheckpointBinding {
            checkpoint_key: "ckpt-1".to_owned(),
            original_tree: TREE_A.to_owned(),
        }];
        let next = publish_scope(&db, &facts, &second, &auth)
            .await
            .expect("second scope publish");
        assert_eq!(next.stream_id, first.stream_id);
        assert_eq!(stream_rows(&db, &scope).await, 1);

        // AC-8: replaying the already-successful second request returns its
        // journal even though the locked tip has moved to the new value.
        let replay_second = publish_scope(&db, &facts, &second, &auth)
            .await
            .expect("second request replay");
        assert!(replay_second.idempotent);
        assert_eq!(replay_second.stream_id, first.stream_id);
        assert_eq!(journal_count(&db, &first.stream_id).await, 2);

        // AC-5: a stale expected-old is rejected and writes nothing.
        let stale = update("", OID_C, Vec::new(), "stale-old");
        assert!(publish_scope(&db, &facts, &stale, &auth).await.is_err());
        assert_eq!(tip(&db, &first.stream_id).await.as_deref(), Some(OID_B));
        assert_eq!(journal_count(&db, &first.stream_id).await, 2);
        assert_eq!(visible_count(&db, &first.stream_id).await, 2);
        let checkpoints = trace_checkpoint::Entity::find()
            .filter(trace_checkpoint::Column::StreamId.eq(&first.stream_id))
            .all(&db)
            .await
            .expect("checkpoints");
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].checkpoint_key, "ckpt-1");
        assert_eq!(checkpoints[0].original_tree, TREE_A);

        // AC-6: the same natural tuple with another hash_kind is rejected.
        let mut sha256 = update("", OID_C, vec![OID_B.to_owned()], "hash-kind");
        sha256.hash_kind = "sha256".to_owned();
        let sha256_facts = ScopeFacts {
            hash_kind: "sha256",
            ..facts
        };
        let mut sha256_auth = auth;
        sha256_auth.expected_generation = auth.expected_generation;
        assert!(
            publish_scope(&db, &sha256_facts, &sha256, &sha256_auth)
                .await
                .is_err()
        );
        assert_eq!(stream_rows(&db, &scope).await, 1);
    }

    #[tokio::test]
    async fn first_scope_atomic_rejection_leaves_no_stream_row() {
        let (_dir, db) = open_db().await;
        let scope = scope_target(&db).await;
        let epoch = Uuid::new_v4().to_string();
        let facts = ScopeFacts {
            stream_id: &scope.stream_id,
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            source_repo_id: "source-opaque",
            epoch_id: &epoch,
            hash_kind: "sha1",
        };
        let token = token(&scope.path);
        let auth = authority(&scope, &token);
        let mut bad = update("", OID_A, Vec::new(), "bad-ref");
        bad.ref_names = vec!["refs/heads/main".to_owned()];
        assert!(publish_scope(&db, &facts, &bad, &auth).await.is_err());
        assert_eq!(stream_rows(&db, &scope).await, 0);
    }
    #[tokio::test]
    async fn first_scope_atomic_concurrent_insert_keeps_one_identity() {
        let (_dir, db) = open_db().await;
        let scope = scope_target(&db).await;
        let epoch = Uuid::new_v4().to_string();
        let id_a = Uuid::new_v4().to_string();
        let id_b = Uuid::new_v4().to_string();
        let facts_a = ScopeFacts {
            stream_id: &id_a,
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            source_repo_id: "source-opaque",
            epoch_id: &epoch,
            hash_kind: "sha1",
        };
        let facts_b = ScopeFacts {
            stream_id: &id_b,
            ..facts_a
        };
        let token = token(&scope.path);
        let auth = authority(&scope, &token);
        let first = update("", OID_A, Vec::new(), "racing-first");
        let second = update("", OID_A, Vec::new(), "racing-first");
        let (left, right) = tokio::join!(
            publish_scope(&db, &facts_a, &first, &auth),
            publish_scope(&db, &facts_b, &second, &auth)
        );
        assert!(
            left.is_ok() != right.is_ok(),
            "exactly one natural-key identity must win"
        );
        assert_eq!(stream_rows(&db, &scope).await, 1);
    }
    #[tokio::test]
    async fn first_scope_atomic_absent_condition_matches_only_an_empty_ref() {
        let (_dir, db) = open_db().await;
        let scope = scope_target(&db).await;
        let epoch = Uuid::new_v4().to_string();
        let facts = ScopeFacts {
            stream_id: &scope.stream_id,
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            source_repo_id: "source-opaque",
            epoch_id: &epoch,
            hash_kind: "sha1",
        };
        let token = token(&scope.path);
        let auth = authority(&scope, &token);
        let mut first = update("", OID_A, Vec::new(), "absent-first");
        first.expected_old_tip = ExpectedOldTip::Absent;
        publish_scope(&db, &facts, &first, &auth)
            .await
            .expect("the ref does not exist yet");
        let mut second = update("", OID_B, vec![OID_A.to_owned()], "absent-second");
        second.expected_old_tip = ExpectedOldTip::Absent;
        assert!(
            publish_scope(&db, &facts, &second, &auth).await.is_err(),
            "an existing ref must not satisfy the absent condition"
        );
        assert_eq!(tip(&db, &scope.stream_id).await.as_deref(), Some(OID_A));
    }
}
