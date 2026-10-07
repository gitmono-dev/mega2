use std::collections::{BTreeMap, HashMap};

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
    jupiter::storage::{
        base_storage::{BaseStorage, StorageConnector},
        view_root_chain::ROOT_CHAIN_HALTED_SQL,
        view_storage::ViewStorage,
    },
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

pub(crate) struct ViewReaderState {
    pub(crate) ready_seq: Option<i64>,
    pub(crate) projected_seq: i64,
    pub(crate) halted: bool,
    pub(crate) view_tip: Option<String>,
}

pub(crate) struct ViewWantState {
    pub(crate) ready_seq: Option<i64>,
    pub(crate) projected_seq: i64,
    pub(crate) halted: bool,
    pub(crate) wants: Vec<(String, Option<i64>)>,
}

pub(crate) struct ViewPackSnapshot {
    pub(crate) ready_seq: Option<i64>,
    pub(crate) halted: bool,
    pub(crate) wants_valid: bool,
    pub(crate) have_tree: Option<String>,
    pub(crate) commits: Vec<ViewPackCommit>,
}

pub(crate) struct ViewPackCommit {
    pub(crate) object_id: String,
    pub(crate) tree_id: String,
    pub(crate) data: Vec<u8>,
}

impl ViewStorage {
    pub(crate) async fn view_pack_snapshot(
        &self,
        filter_pk: i64,
        wants: &[String],
        haves: &[String],
    ) -> Result<Option<ViewPackSnapshot>, MegaError> {
        let rows = self
            .get_connection()
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                format!(
                    "WITH state AS MATERIALIZED ( \
                         SELECT f.id, f.ready_seq, f.projected_seq, \
                                {ROOT_CHAIN_HALTED_SQL} AS halted \
                         FROM mega_view_filter f WHERE f.id = $1 \
                     ), wanted AS ( \
                         SELECT mapped.seq_from FROM state s \
                         CROSS JOIN LATERAL unnest($2::text[]) AS w(oid) \
                         LEFT JOIN LATERAL ( \
                             SELECT m.seq_from FROM mega_view_commit_map m \
                             WHERE m.filter_pk = s.id AND m.view_commit = w.oid \
                               AND m.seq_from <= s.projected_seq \
                             ORDER BY m.seq_from LIMIT 1 \
                         ) mapped ON TRUE \
                     ), bounds AS ( \
                         SELECT (SELECT max(seq_from) FROM wanted) AS want_seq, \
                                (SELECT coalesce(bool_and(seq_from IS NOT NULL), FALSE) FROM wanted) \
                                    AS wants_valid, \
                                (SELECT coalesce(max(m.seq_from), 0) FROM state s \
                                 JOIN mega_view_commit_map m ON m.filter_pk = s.id \
                                 WHERE m.view_commit = ANY($3::text[]) \
                                   AND m.seq_from <= s.projected_seq) AS have_seq \
                     ) \
                     SELECT s.ready_seq, s.halted, b.wants_valid, \
                            h.view_tree AS have_tree, m.view_commit, m.view_tree, o.data \
                     FROM state s CROSS JOIN bounds b \
                     LEFT JOIN mega_view_commit_map h \
                       ON h.filter_pk = s.id AND h.seq_from = b.have_seq \
                     LEFT JOIN mega_view_commit_map m \
                       ON m.filter_pk = s.id AND m.seq_from > b.have_seq \
                          AND m.seq_from <= b.want_seq AND m.view_commit IS NOT NULL \
                     LEFT JOIN mega_view_object o \
                       ON o.object_id = m.view_commit AND o.kind = 1 \
                     ORDER BY m.seq_from"
                ),
                [
                    Value::from(filter_pk),
                    Value::from(wants.to_vec()),
                    Value::from(haves.to_vec()),
                ],
            ))
            .await?;
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let mut snapshot = ViewPackSnapshot {
            ready_seq: first.try_get("", "ready_seq")?,
            halted: first.try_get("", "halted")?,
            wants_valid: first.try_get("", "wants_valid")?,
            have_tree: first.try_get("", "have_tree")?,
            commits: Vec::new(),
        };
        if snapshot.halted || snapshot.ready_seq.is_none() || !snapshot.wants_valid {
            return Ok(Some(snapshot));
        }
        for row in rows {
            let Some(object_id) = row.try_get::<Option<String>>("", "view_commit")? else {
                continue;
            };
            let data: Option<Vec<u8>> = row.try_get("", "data")?;
            snapshot.commits.push(ViewPackCommit {
                tree_id: row.try_get("", "view_tree")?,
                data: data.ok_or_else(|| {
                    MegaError::Other(format!("view commit object missing: {object_id}"))
                })?,
                object_id,
            });
        }
        Ok(Some(snapshot))
    }

    pub(crate) async fn view_pack_view_trees(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<u8>>, MegaError> {
        let mut trees = HashMap::new();
        for chunk in ids.chunks(<BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE) {
            let rows = self
                .get_connection()
                .query_all_raw(Statement::from_sql_and_values(
                    sea_orm::DbBackend::Postgres,
                    "SELECT object_id, data FROM mega_view_object \
                     WHERE kind = 2 AND object_id = ANY($1::text[])",
                    [Value::from(chunk.to_vec())],
                ))
                .await?;
            for row in rows {
                trees.insert(row.try_get("", "object_id")?, row.try_get("", "data")?);
            }
        }
        Ok(trees)
    }

    #[cfg(test)]
    pub(crate) async fn view_pack_commits(
        &self,
        filter_pk: i64,
        have_seq: i64,
        want_seq: i64,
    ) -> Result<Vec<ViewPackCommit>, MegaError> {
        let rows = self
            .get_connection()
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT m.view_commit, m.view_tree, o.data \
                 FROM mega_view_commit_map m \
                 LEFT JOIN mega_view_object o ON o.object_id = m.view_commit AND o.kind = 1 \
                 WHERE m.filter_pk = $1 AND m.seq_from > $2 AND m.seq_from <= $3 \
                   AND m.view_commit IS NOT NULL \
                 ORDER BY m.seq_from",
                [
                    Value::from(filter_pk),
                    Value::from(have_seq),
                    Value::from(want_seq),
                ],
            ))
            .await?;
        rows.into_iter()
            .map(|row| {
                let object_id: String = row.try_get("", "view_commit")?;
                let data: Option<Vec<u8>> = row.try_get("", "data")?;
                Ok(ViewPackCommit {
                    tree_id: row.try_get("", "view_tree")?,
                    data: data.ok_or_else(|| {
                        MegaError::Other(format!("view commit object missing: {object_id}"))
                    })?,
                    object_id,
                })
            })
            .collect()
    }

    pub(crate) async fn view_reader_state(
        &self,
        filter_pk: i64,
    ) -> Result<Option<ViewReaderState>, MegaError> {
        let row = self
            .get_connection()
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                format!(
                    "SELECT f.ready_seq, f.projected_seq, {ROOT_CHAIN_HALTED_SQL} AS halted, \
                     (SELECT m.view_commit FROM mega_view_commit_map m \
                      WHERE m.filter_pk = f.id AND m.seq_from <= f.projected_seq \
                      ORDER BY m.seq_from DESC LIMIT 1) AS view_tip \
                     FROM mega_view_filter f WHERE f.id = $1"
                ),
                [Value::from(filter_pk)],
            ))
            .await?;
        row.map(|row| {
            Ok(ViewReaderState {
                ready_seq: row.try_get("", "ready_seq")?,
                projected_seq: row.try_get("", "projected_seq")?,
                halted: row.try_get("", "halted")?,
                view_tip: row.try_get("", "view_tip")?,
            })
        })
        .transpose()
    }

    pub(crate) async fn view_want_state(
        &self,
        filter_pk: i64,
        wants: &[String],
    ) -> Result<Option<ViewWantState>, MegaError> {
        let rows = self
            .get_connection()
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                format!(
                    "WITH state AS MATERIALIZED ( \
                         SELECT f.id, f.ready_seq, f.projected_seq, \
                                {ROOT_CHAIN_HALTED_SQL} AS halted \
                         FROM mega_view_filter f WHERE f.id = $1 \
                     ) \
                     SELECT state.ready_seq, state.projected_seq, state.halted, \
                            wanted.oid, mapped.seq_from \
                     FROM state \
                     LEFT JOIN LATERAL unnest($2::text[]) WITH ORDINALITY \
                         AS wanted(oid, position) ON TRUE \
                     LEFT JOIN LATERAL ( \
                         SELECT m.seq_from FROM mega_view_commit_map m \
                         WHERE m.filter_pk = state.id AND m.view_commit = wanted.oid \
                         ORDER BY m.seq_from LIMIT 1 \
                     ) mapped ON TRUE \
                     ORDER BY wanted.position"
                ),
                [Value::from(filter_pk), Value::from(wants.to_vec())],
            ))
            .await?;
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let mut result = ViewWantState {
            ready_seq: first.try_get("", "ready_seq")?,
            projected_seq: first.try_get("", "projected_seq")?,
            halted: first.try_get("", "halted")?,
            wants: Vec::with_capacity(wants.len()),
        };
        for row in rows {
            let oid: Option<String> = row.try_get("", "oid")?;
            if let Some(oid) = oid {
                result.wants.push((oid, row.try_get("", "seq_from")?));
            }
        }
        Ok(Some(result))
    }

    pub(crate) async fn view_commit_exists(&self, filter_pk: i64, hash: &str) -> bool {
        self.get_connection()
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT EXISTS (SELECT 1 FROM mega_view_commit_map \
                 WHERE filter_pk = $1 AND view_commit = $2) AS present",
                [Value::from(filter_pk), Value::from(hash.to_owned())],
            ))
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get("", "present").ok())
            .unwrap_or(false)
    }

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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use sea_orm::ConnectionTrait;

    use super::*;
    use crate::jupiter::{
        storage::{base_storage::BaseStorage, init::database_connection},
        tests::test_db_config,
    };

    #[tokio::test]
    async fn view_readers_single_statement() {
        let temp = tempfile::tempdir().unwrap();
        let (db_config, _schema) = test_db_config(temp.path()).await;
        let mut db = database_connection(&db_config).await.unwrap();
        db.execute_unprepared(&format!(
            "INSERT INTO mega_view_filter \
             (id, filter_id, canonical_spec, algo_version, object_format, src_paths, \
              push_enabled, projected_seq, ready_seq, created_at) \
             VALUES (1, '{}', ':/repo', 1, 'sha1', '[]'::jsonb, false, 0, NULL, now())",
            "a".repeat(64)
        ))
        .await
        .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let callback_count = count.clone();
        db.set_metric_callback(move |_| {
            callback_count.fetch_add(1, Ordering::Relaxed);
        });
        let view = ViewStorage::new(BaseStorage::new(Arc::new(db)));

        for ready in [false, true] {
            if ready {
                view.get_connection()
                    .execute_unprepared("UPDATE mega_view_filter SET ready_seq = 0 WHERE id = 1")
                    .await
                    .unwrap();
            }
            count.store(0, Ordering::Relaxed);
            let state = view.view_reader_state(1).await.unwrap().unwrap();
            assert_eq!(state.ready_seq.is_some(), ready);
            assert!(!state.halted);
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }

        view.get_connection()
            .execute_unprepared(&format!(
                "INSERT INTO mega_view_root_chain (seq, commit_id, tree_id) \
                 VALUES (1, '{}', '{}'); \
                 INSERT INTO mega_view_root_chain_scan \
                 (pos, commit_id, tree_id, parent_count, first_parent) \
                 VALUES (1, '{}', '{}', 0, NULL)",
                "a".repeat(40),
                "b".repeat(40),
                "c".repeat(40),
                "d".repeat(40)
            ))
            .await
            .unwrap();
        count.store(0, Ordering::Relaxed);
        let halted = view.view_reader_state(1).await.unwrap().unwrap();
        assert!(halted.halted);
        assert_eq!(count.load(Ordering::Relaxed), 1);

        for wanted in [vec!["e".repeat(40)], vec!["e".repeat(40); 100]] {
            count.store(0, Ordering::Relaxed);
            let state = view.view_want_state(1, &wanted).await.unwrap().unwrap();
            assert_eq!(state.wants.len(), wanted.len());
            assert!(state.halted);
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }
    }
}
