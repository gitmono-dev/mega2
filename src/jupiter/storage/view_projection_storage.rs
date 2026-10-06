use std::collections::BTreeMap;

use chrono::Utc;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, Statement, Value, sea_query::OnConflict,
};

use crate::{
    callisto::{
        mega_refs, mega_view_commit_map, mega_view_filter, mega_view_object, mega_view_object_ref,
        mega_view_root_chain,
    },
    ceres::view::{
        project::{PreviousProjection, Segment},
        tree::FilterOutput,
    },
    common::{errors::MegaError, utils::MEGA_BRANCH_NAME},
    jupiter::storage::{base_storage::StorageConnector, view_storage::ViewStorage},
};

#[derive(Clone, Debug)]
pub(crate) struct ProjectionFilter {
    pub(crate) id: i64,
    pub(crate) filter_id: String,
    pub(crate) canonical_spec: String,
    pub(crate) projected_seq: i64,
    pub(crate) ready_seq: Option<i64>,
    pub(crate) warming_since: Option<chrono::NaiveDateTime>,
}

impl From<mega_view_filter::Model> for ProjectionFilter {
    fn from(row: mega_view_filter::Model) -> Self {
        Self {
            id: row.id,
            filter_id: row.filter_id,
            canonical_spec: row.canonical_spec,
            projected_seq: row.projected_seq,
            ready_seq: row.ready_seq,
            warming_since: row.warming_since,
        }
    }
}

impl ViewStorage {
    pub(crate) async fn warming_filter_count(&self) -> Result<u64, MegaError> {
        Ok(mega_view_filter::Entity::find()
            .filter(mega_view_filter::Column::WarmingSince.is_not_null())
            .count(self.get_connection())
            .await?)
    }

    pub(crate) async fn projection_filter(
        &self,
        txn: &DatabaseTransaction,
        filter_pk: i64,
    ) -> Result<Option<ProjectionFilter>, MegaError> {
        Ok(mega_view_filter::Entity::find_by_id(filter_pk)
            .one(txn)
            .await?
            .map(Into::into))
    }

    pub(crate) async fn projection_tip(&self, txn: &DatabaseTransaction) -> Result<i64, MegaError> {
        Ok(mega_view_root_chain::Entity::find()
            .order_by_desc(mega_view_root_chain::Column::Seq)
            .one(txn)
            .await?
            .map_or(0, |row| row.seq))
    }

    pub(crate) async fn projection_rows(
        &self,
        txn: &DatabaseTransaction,
        first: i64,
        last: i64,
    ) -> Result<Vec<mega_view_root_chain::Model>, MegaError> {
        Ok(mega_view_root_chain::Entity::find()
            .filter(mega_view_root_chain::Column::Seq.gte(first))
            .filter(mega_view_root_chain::Column::Seq.lte(last))
            .order_by_asc(mega_view_root_chain::Column::Seq)
            .all(txn)
            .await?)
    }

    pub(crate) async fn previous_projection(
        &self,
        txn: &DatabaseTransaction,
        filter_pk: i64,
        seq: i64,
        empty_tree: &str,
    ) -> Result<PreviousProjection, MegaError> {
        let row = mega_view_commit_map::Entity::find()
            .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
            .filter(mega_view_commit_map::Column::SeqFrom.lte(seq))
            .order_by_desc(mega_view_commit_map::Column::SeqFrom)
            .one(txn)
            .await?;
        Ok(row.map_or_else(
            || PreviousProjection {
                view_commit: None,
                view_tree: empty_tree.to_owned(),
            },
            |row| PreviousProjection {
                view_commit: row.view_commit,
                view_tree: row.view_tree,
            },
        ))
    }

    pub(crate) async fn write_projection_batch(
        &self,
        txn: &DatabaseTransaction,
        filter_pk: i64,
        segments: &[Segment],
        outputs: &BTreeMap<i64, FilterOutput>,
        projected_seq: i64,
    ) -> Result<(), MegaError> {
        let maps = segments
            .iter()
            .map(|segment| mega_view_commit_map::ActiveModel {
                filter_pk: Set(filter_pk),
                seq_from: Set(segment.seq),
                view_commit: Set(segment
                    .view_commit
                    .as_ref()
                    .map(|commit| commit.id.to_string())),
                view_tree: Set(segment.view_tree.clone()),
            })
            .collect::<Vec<_>>();
        insert_ignore::<mega_view_commit_map::Entity, _>(txn, maps).await?;

        let mut objects = BTreeMap::<String, (i16, Vec<u8>)>::new();
        for segment in segments {
            if let Some(commit) = &segment.view_commit {
                objects.insert(commit.id.to_string(), (1, commit.bytes.clone()));
            }
        }
        for output in outputs.values() {
            for (object_id, data) in &output.trees {
                objects
                    .entry(object_id.clone())
                    .or_insert((2, data.clone()));
            }
        }

        let object_models = objects
            .iter()
            .map(|(object_id, (kind, data))| mega_view_object::ActiveModel {
                object_id: Set(object_id.clone()),
                kind: Set(*kind),
                data: Set(data.clone()),
                created_at: Set(Utc::now().naive_utc()),
                gc_marked_at: Set(None),
            })
            .collect::<Vec<_>>();
        insert_ignore::<mega_view_object::Entity, _>(txn, object_models).await?;

        let refs = objects
            .keys()
            .map(|object_id| mega_view_object_ref::ActiveModel {
                filter_pk: Set(filter_pk),
                object_id: Set(object_id.clone()),
            })
            .collect::<Vec<_>>();
        insert_ignore::<mega_view_object_ref::Entity, _>(txn, refs).await?;

        for ids in objects
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .chunks(<crate::jupiter::storage::base_storage::BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE)
        {
            if ids.is_empty() {
                continue;
            }
            let placeholders = (1..=ids.len())
                .map(|index| format!("${index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let values = ids.iter().cloned().map(Value::from).collect::<Vec<_>>();
            txn.execute_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                format!(
                    "UPDATE mega_view_object SET gc_marked_at = NULL WHERE gc_marked_at IS NOT NULL AND object_id IN ({placeholders})"
                ),
                values,
            ))
            .await?;
        }

        txn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DbBackend::Postgres,
            "UPDATE mega_view_filter SET projected_seq = GREATEST(projected_seq, $1) WHERE id = $2",
            [Value::from(projected_seq), Value::from(filter_pk)],
        ))
        .await?;
        Ok(())
    }

    pub(crate) async fn mark_ready_if_covered(
        &self,
        txn: &DatabaseTransaction,
        filter_pk: i64,
        tip: i64,
    ) -> Result<bool, MegaError> {
        let Some(tail) = mega_view_root_chain::Entity::find_by_id(tip)
            .one(txn)
            .await?
        else {
            return Ok(false);
        };
        let main = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
            .filter(mega_refs::Column::IsCl.eq(false))
            .one(txn)
            .await?;
        if main
            .as_ref()
            .is_none_or(|row| row.ref_commit_hash != tail.commit_id)
        {
            return Ok(false);
        }
        txn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DbBackend::Postgres,
            "UPDATE mega_view_filter \
             SET ready_seq = COALESCE(ready_seq, $1), \
                 warming_since = CASE WHEN ready_seq IS NULL THEN NULL ELSE warming_since END \
             WHERE id = $2",
            [Value::from(tip), Value::from(filter_pk)],
        ))
        .await?;
        Ok(true)
    }
}

async fn insert_ignore<E, A>(txn: &DatabaseTransaction, rows: Vec<A>) -> Result<(), MegaError>
where
    E: EntityTrait,
    A: sea_orm::ActiveModelTrait<Entity = E> + From<<E as EntityTrait>::Model> + Send + Clone,
{
    for batch in rows.chunks(
        <crate::jupiter::storage::base_storage::BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE,
    ) {
        if batch.is_empty() {
            continue;
        }
        E::insert_many(batch.to_vec())
            .on_conflict(OnConflict::new().do_nothing().to_owned())
            // A completely conflicting batch is expected when two filters
            // materialize the same projected object. `exec` requests a
            // returned row and treats that valid outcome as
            // `RecordNotInserted`; execute the insert without a RETURNING
            // clause so `ON CONFLICT DO NOTHING` keeps the transaction usable.
            .exec_without_returning(txn)
            .await?;
    }
    Ok(())
}
