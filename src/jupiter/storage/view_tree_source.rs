use std::collections::{HashMap, HashSet};

use git_internal::{hash::HashKind, internal::object::tree::Tree};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

use crate::{
    callisto::mega_view_object,
    ceres::view::tree_source::{
        MissingObject, MissingObjectReason, TreeSource, empty_tree_id, parse_tree_bytes,
    },
    common::errors::MegaError,
    jupiter::storage::{
        base_storage::{BaseStorage, StorageConnector},
        mono_storage::MonoStorage,
    },
};

/// A one-batch database-backed tree source.
///
/// Database work is isolated to [`Self::prefetch`]; the `TreeSource`
/// implementation only reads this instance's completed prefetch state.
pub(crate) struct ViewTreeSource<'a, C: ConnectionTrait> {
    mono_storage: &'a MonoStorage,
    connection: &'a C,
    kind: HashKind,
    empty_tree_id: String,
    tree_bytes: HashMap<String, Vec<u8>>,
    missing: HashSet<String>,
}

impl<'a, C: ConnectionTrait> ViewTreeSource<'a, C> {
    pub(crate) fn new(
        mono_storage: &'a MonoStorage,
        connection: &'a C,
        kind: HashKind,
    ) -> Result<Self, MegaError> {
        let empty_tree_id = empty_tree_id(kind)
            .map_err(|err| MegaError::Other(format!("failed to calculate empty tree id: {err}")))?
            .to_string();
        Ok(Self {
            mono_storage,
            connection,
            kind,
            empty_tree_id,
            tree_bytes: HashMap::new(),
            missing: HashSet::new(),
        })
    }

    /// Loads ids into this instance. A failed query leaves all previous
    /// prefetch state intact and makes no newly requested id observable.
    pub(crate) async fn prefetch(&mut self, ids: &[String]) -> Result<(), MegaError> {
        let requested = self.pending_ids(ids);
        if requested.is_empty() {
            return Ok(());
        }

        let mut loaded = HashMap::new();
        for chunk in requested.chunks(<BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE) {
            let rows = mega_view_object::Entity::find()
                .filter(mega_view_object::Column::Kind.eq(2_i16))
                .filter(mega_view_object::Column::ObjectId.is_in(chunk))
                .all(self.connection)
                .await?;
            loaded.extend(rows.into_iter().map(|row| (row.object_id, row.data)));
        }

        let remaining: Vec<String> = requested
            .iter()
            .filter(|id| !loaded.contains_key(*id))
            .cloned()
            .collect();
        for tree in self
            .mono_storage
            .get_trees_by_hashes_fallible(self.connection, &remaining)
            .await?
        {
            loaded.insert(tree.tree_id, tree.sub_trees);
        }

        let missing = requested
            .into_iter()
            .filter(|id| !loaded.contains_key(id))
            .collect::<HashSet<_>>();
        self.tree_bytes.extend(loaded);
        self.missing.extend(missing);
        Ok(())
    }

    fn pending_ids(&self, ids: &[String]) -> Vec<String> {
        let mut seen = HashSet::with_capacity(ids.len());
        ids.iter()
            .filter(|id| {
                *id != &self.empty_tree_id
                    && !self.tree_bytes.contains_key(*id)
                    && !self.missing.contains(*id)
                    && seen.insert((*id).clone())
            })
            .cloned()
            .collect()
    }
}

impl<C: ConnectionTrait> TreeSource for ViewTreeSource<'_, C> {
    fn read_tree(&self, tree_id: &str) -> Result<Tree, MissingObject> {
        if self.missing.contains(tree_id) {
            return Err(missing(tree_id, MissingObjectReason::Absent));
        }
        if let Some(bytes) = self.tree_bytes.get(tree_id) {
            return parse_tree_bytes(self.kind, tree_id, bytes);
        }
        Err(missing(tree_id, MissingObjectReason::Unprefetched))
    }
}

fn missing(tree_id: &str, reason: MissingObjectReason) -> MissingObject {
    MissingObject {
        tree_id: tree_id.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    };

    use git_internal::{
        hash::{HashKind, set_hash_kind_for_test},
        internal::object::{
            ObjectTrait,
            blob::Blob,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ConnectionTrait, DatabaseConnection, TransactionTrait,
    };

    use super::ViewTreeSource;
    use crate::{
        callisto::{mega_tree, mega_view_object},
        ceres::view::tree_source::{MissingObjectReason, parse_tree_bytes, read_tree},
        jupiter::{
            migration::apply_migrations,
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                init::database_connection,
                mono_storage::MonoStorage,
            },
            tests::{test_db_config, test_db_connection},
        },
    };

    static TREE_ROW_ID: AtomicI64 = AtomicI64::new(1);

    fn id(kind: HashKind, digit: char) -> String {
        digit.to_string().repeat(kind.hex_len())
    }

    fn opposite(kind: HashKind) -> HashKind {
        match kind {
            HashKind::Sha1 => HashKind::Sha256,
            HashKind::Sha256 | HashKind::Blake3 => HashKind::Sha1,
        }
    }

    fn tree_bytes(kind: HashKind, label: &str) -> Vec<u8> {
        let blob = Blob::from_content_bytes_with_kind(kind, label.as_bytes().to_vec()).unwrap();
        Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(
                TreeItemMode::Blob,
                blob.id,
                "entry".to_owned(),
            )],
        )
        .unwrap()
        .to_data()
        .unwrap()
    }

    async fn storage() -> (tempfile::TempDir, DatabaseConnection, MonoStorage) {
        let temp = tempfile::tempdir().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let mono = MonoStorage {
            base: BaseStorage::new(Arc::new(db.clone())),
        };
        (temp, db, mono)
    }

    async fn insert_view_tree(db: &DatabaseConnection, object_id: &str, kind: i16, data: Vec<u8>) {
        mega_view_object::ActiveModel {
            object_id: Set(object_id.to_owned()),
            kind: Set(kind),
            data: Set(data),
            created_at: Set(chrono::Utc::now().naive_utc()),
            gc_marked_at: Set(None),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn insert_tree(db: &DatabaseConnection, tree_id: &str, data: Vec<u8>) {
        mega_tree::ActiveModel {
            id: Set(TREE_ROW_ID.fetch_add(1, Ordering::Relaxed)),
            tree_id: Set(tree_id.to_owned()),
            sub_trees: Set(data),
            size: Set(0),
            created_at: Set(chrono::Utc::now().naive_utc()),
            pack_id: Set(String::new()),
            pack_offset: Set(0),
            commit_id: Set(String::new()),
        }
        .insert(db)
        .await
        .unwrap();
    }

    fn assert_missing<C: ConnectionTrait>(
        kind: HashKind,
        source: &ViewTreeSource<'_, C>,
        tree_id: &str,
        reason: MissingObjectReason,
    ) {
        let error = read_tree(kind, source, tree_id).unwrap_err();
        assert_eq!(error.tree_id, tree_id);
        assert_eq!(error.reason, reason);
    }

    fn assert_prefetched<C: ConnectionTrait>(
        kind: HashKind,
        source: &ViewTreeSource<'_, C>,
        ids: &[String],
    ) {
        for tree_id in ids {
            let tree = read_tree(kind, source, tree_id).unwrap();
            assert_eq!(tree.id.to_string(), *tree_id);
            assert_eq!(tree.tree_items.len(), 1);
        }
    }

    fn assert_tree_items<C: ConnectionTrait>(
        kind: HashKind,
        source: &ViewTreeSource<'_, C>,
        tree_id: &str,
        expected: &[TreeItem],
    ) {
        assert_eq!(
            read_tree(kind, source, tree_id).unwrap().tree_items,
            expected
        );
    }

    #[tokio::test]
    async fn lookup_order_view_object_first() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            let (_temp, db, mono) = storage().await;
            let view_id = id(kind, '1');
            let l0_id = id(kind, '2');
            let precedence_id = id(kind, '3');
            let kind_one_id = id(kind, '4');
            insert_view_tree(&db, &view_id, 2, tree_bytes(kind, "view")).await;
            let l0_bytes = tree_bytes(kind, "l0");
            let expected_l0_items = parse_tree_bytes(kind, &l0_id, &l0_bytes)
                .unwrap()
                .tree_items;
            insert_tree(&db, &l0_id, l0_bytes).await;
            insert_view_tree(&db, &precedence_id, 2, tree_bytes(kind, "preferred")).await;
            insert_tree(&db, &precedence_id, b"bad tree bytes".to_vec()).await;
            insert_view_tree(&db, &kind_one_id, 1, b"not a tree".to_vec()).await;
            insert_tree(&db, &kind_one_id, tree_bytes(kind, "kind one fallback")).await;

            let ids = vec![
                view_id.clone(),
                l0_id.clone(),
                precedence_id.clone(),
                kind_one_id.clone(),
            ];
            if kind == HashKind::Sha256 {
                let txn = db.begin().await.unwrap();
                let mut transaction_source = ViewTreeSource::new(&mono, &txn, kind).unwrap();
                transaction_source.prefetch(&ids).await.unwrap();
                assert_prefetched(kind, &transaction_source, &ids);
                assert_tree_items(kind, &transaction_source, &l0_id, &expected_l0_items);
                drop(transaction_source);
                txn.rollback().await.unwrap();
            } else {
                let mut source = ViewTreeSource::new(&mono, &db, kind).unwrap();
                source.prefetch(&ids).await.unwrap();
                assert_prefetched(kind, &source, &ids);
                assert_tree_items(kind, &source, &l0_id, &expected_l0_items);
            }
        }
    }

    #[tokio::test]
    async fn read_tree_outcomes_over_db_source() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            let (_temp, db, mono) = storage().await;
            let absent = id(kind, 'a');
            let bad_tree = id(kind, 'b');
            let bad_view = id(kind, 'c');
            let wrong_kind = id(opposite(kind), 'd');
            let unprefetched = id(kind, 'e');
            let mut truncated_tree = tree_bytes(kind, "truncated tree");
            truncated_tree.pop();
            let mut truncated_view = tree_bytes(kind, "truncated view");
            truncated_view.pop();
            insert_tree(&db, &bad_tree, truncated_tree).await;
            insert_view_tree(&db, &bad_view, 2, truncated_view).await;
            insert_tree(&db, &wrong_kind, tree_bytes(kind, "wrong id length")).await;

            let mut source = ViewTreeSource::new(&mono, &db, kind).unwrap();
            source
                .prefetch(&[
                    absent.clone(),
                    bad_tree.clone(),
                    bad_view.clone(),
                    wrong_kind.clone(),
                ])
                .await
                .unwrap();
            assert_missing(kind, &source, &absent, MissingObjectReason::Absent);
            assert_missing(kind, &source, &bad_tree, MissingObjectReason::Malformed);
            assert_missing(kind, &source, &bad_view, MissingObjectReason::Malformed);
            assert_missing(kind, &source, &wrong_kind, MissingObjectReason::Malformed);
            assert_missing(
                kind,
                &source,
                &unprefetched,
                MissingObjectReason::Unprefetched,
            );

            insert_tree(&db, &absent, tree_bytes(kind, "restored")).await;
            source
                .prefetch(std::slice::from_ref(&absent))
                .await
                .unwrap();
            assert_missing(kind, &source, &absent, MissingObjectReason::Absent);
            let mut refreshed = ViewTreeSource::new(&mono, &db, kind).unwrap();
            refreshed
                .prefetch(std::slice::from_ref(&absent))
                .await
                .unwrap();
            assert_eq!(
                read_tree(kind, &refreshed, &absent).unwrap().id.to_string(),
                absent
            );
        }
    }

    #[tokio::test]
    async fn prefetch_statement_count() {
        let temp = tempfile::tempdir().unwrap();
        let (config, _schema) = test_db_config(temp.path()).await;
        let counter = Arc::new(AtomicUsize::new(0));
        let callback_counter = counter.clone();
        let mut db = database_connection(&config).await.unwrap();
        db.set_metric_callback(move |_| {
            callback_counter.fetch_add(1, Ordering::Relaxed);
        });
        let mono = MonoStorage {
            base: BaseStorage::new(Arc::new(db.clone())),
        };
        let kind = HashKind::Sha1;
        let view_ids: Vec<_> = (0..300).map(|index| format!("1{index:039x}")).collect();
        let tree_ids: Vec<_> = (0..1000).map(|index| format!("2{index:039x}")).collect();
        let absent_ids: Vec<_> = (0..200).map(|index| format!("3{index:039x}")).collect();
        for tree_id in &view_ids {
            insert_view_tree(&db, tree_id, 2, tree_bytes(kind, tree_id)).await;
        }
        for tree_id in &tree_ids {
            insert_tree(&db, tree_id, tree_bytes(kind, tree_id)).await;
        }
        let mut ids = view_ids.clone();
        ids.extend(tree_ids.clone());
        ids.extend(absent_ids);

        let mut source = ViewTreeSource::new(&mono, &db, kind).unwrap();
        counter.store(0, Ordering::Relaxed);
        source.prefetch(&ids).await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 4);
        counter.store(0, Ordering::Relaxed);
        source.prefetch(&ids).await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        for tree_id in &ids {
            let _ = read_tree(kind, &source, tree_id);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        let mut only_view = ViewTreeSource::new(&mono, &db, kind).unwrap();
        counter.store(0, Ordering::Relaxed);
        only_view.prefetch(&view_ids).await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        let mut empty = ViewTreeSource::new(&mono, &db, kind).unwrap();
        counter.store(0, Ordering::Relaxed);
        empty.prefetch(&[]).await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        let mut empty_tree = ViewTreeSource::new(&mono, &db, kind).unwrap();
        counter.store(0, Ordering::Relaxed);
        empty_tree
            .prefetch(&[crate::ceres::view::tree_source::empty_tree_id(kind)
                .unwrap()
                .to_string()])
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn prefetch_query_error_keeps_state() {
        let (_temp, db, mono) = storage().await;
        let kind = HashKind::Sha1;
        let cached = id(kind, '1');
        let cached_absent = id(kind, '2');
        let cached_view = id(kind, '7');
        let view_after_error = id(kind, '3');
        let tree_after_error = id(kind, '4');
        let absent_after_error = id(kind, '5');
        let view_column_error = id(kind, '6');
        insert_tree(&db, &cached, tree_bytes(kind, "cached")).await;
        insert_view_tree(&db, &cached_view, 2, tree_bytes(kind, "cached view")).await;
        insert_view_tree(&db, &view_after_error, 2, tree_bytes(kind, "new view")).await;
        insert_tree(&db, &tree_after_error, tree_bytes(kind, "new tree")).await;
        insert_view_tree(&db, &view_column_error, 2, tree_bytes(kind, "view error")).await;

        let mut source = ViewTreeSource::new(&mono, &db, kind).unwrap();
        source
            .prefetch(&[cached.clone(), cached_view.clone(), cached_absent.clone()])
            .await
            .unwrap();
        assert_eq!(
            read_tree(kind, &source, &cached).unwrap().id.to_string(),
            cached
        );
        assert_eq!(
            read_tree(kind, &source, &cached_view)
                .unwrap()
                .id
                .to_string(),
            cached_view
        );
        assert_missing(kind, &source, &cached_absent, MissingObjectReason::Absent);

        db.execute_unprepared("ALTER TABLE mega_tree RENAME COLUMN sub_trees TO hp29_gone")
            .await
            .unwrap();
        assert!(
            source
                .prefetch(&[
                    view_after_error.clone(),
                    tree_after_error.clone(),
                    absent_after_error.clone(),
                ])
                .await
                .is_err()
        );
        assert_eq!(
            read_tree(kind, &source, &cached).unwrap().id.to_string(),
            cached
        );
        assert_eq!(
            read_tree(kind, &source, &cached_view)
                .unwrap()
                .id
                .to_string(),
            cached_view
        );
        assert_missing(kind, &source, &cached_absent, MissingObjectReason::Absent);
        for tree_id in [&view_after_error, &tree_after_error, &absent_after_error] {
            assert_missing(kind, &source, tree_id, MissingObjectReason::Unprefetched);
        }

        db.execute_unprepared("ALTER TABLE mega_tree RENAME COLUMN hp29_gone TO sub_trees")
            .await
            .unwrap();
        db.execute_unprepared("ALTER TABLE mega_view_object RENAME COLUMN data TO hp29_gone")
            .await
            .unwrap();
        assert!(
            source
                .prefetch(std::slice::from_ref(&view_column_error))
                .await
                .is_err()
        );
        assert_missing(
            kind,
            &source,
            &view_column_error,
            MissingObjectReason::Unprefetched,
        );
    }
}
