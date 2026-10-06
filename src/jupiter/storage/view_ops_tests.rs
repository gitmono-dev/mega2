//! Executable contract for the P0 view-maintenance SQL in the deployment guide.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{NaiveDate, Utc};
use git_internal::hash::HashKind;
use regex::Regex;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection,
    DbBackend, DbErr, EntityTrait, QueryFilter, Statement,
};

use crate::{
    callisto::{mega_tree, mega_view, mega_view_filter, mega_view_register_log},
    ceres::view::filter::parse_for_registration,
    config::testing::isolated_config,
    jupiter::{
        migration::apply_migrations,
        service::{
            view_metrics::ViewMetrics,
            view_projection_service::{CatchUpOutcome, ViewProjectionService},
        },
        storage::{
            Storage,
            git_db_storage::fu18_support::single_connection,
            object_storage::mock_object_storage,
            view_root_chain::RootChainOutcome,
            view_storage::ViewLockMode,
            view_test_fixtures::{
                cas_fixture_main, root_tree_from_paths, seed_linear_root_history_with_trees,
                seed_unrelated_root_history,
            },
        },
        tests::test_db_connection,
    },
};

const OPS_DOC: &str = include_str!("../../../docs/deploy-trunk.md");
const SNAPSHOT_TABLES: [(&str, &str); 11] = [
    ("mega_view_filter", "id"),
    ("mega_view", "id"),
    ("mega_view_root_chain", "seq"),
    ("mega_view_root_chain_scan", "pos"),
    ("mega_view_commit_map", "filter_pk, seq_from"),
    ("mega_view_object", "object_id"),
    ("mega_view_object_ref", "filter_pk, object_id"),
    ("mega_view_register_log", "id"),
    ("mega_commit", "id"),
    ("mega_tree", "id"),
    ("mega_refs", "id"),
];

type Snapshot = BTreeMap<&'static str, Vec<String>>;

struct OpsFixture {
    _temp: tempfile::TempDir,
    db: DatabaseConnection,
    filter_a: i64,
    filter_b: i64,
    filter_a_id: String,
}

fn fixed_time() -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
}

fn ops_block(name: &str) -> String {
    let marker = format!("-- view-ops: {name}");
    let lines = OPS_DOC.split_inclusive('\n').collect::<Vec<_>>();
    let mut matches = Vec::new();
    for index in 0..lines.len().saturating_sub(1) {
        if lines[index].trim_end_matches('\n') != "```sql"
            || lines[index + 1].trim_end_matches('\n') != marker
        {
            continue;
        }
        let close = lines[index + 2..]
            .iter()
            .position(|line| line.trim_end_matches('\n') == "```")
            .map(|offset| index + 2 + offset)
            .unwrap_or_else(|| panic!("missing closing fence for {marker}"));
        matches.push(lines[index + 1..close].concat());
    }
    assert_eq!(
        matches.len(),
        1,
        "marker must identify exactly one SQL block"
    );
    matches.pop().unwrap()
}

fn ops_sql(name: &str) -> String {
    ops_block(name)
}

fn psql_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut statement = String::new();
    for raw in sql.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        if !statement.is_empty() {
            statement.push(' ');
        }
        statement.push_str(line);
        if line.ends_with(';') {
            statements.push(std::mem::take(&mut statement));
        }
    }
    assert!(
        statement.is_empty(),
        "unterminated SQL statement: {statement}"
    );
    statements
}

async fn run_like_psql(db: &DatabaseConnection, sql: &str) -> Result<(), DbErr> {
    let single = single_connection(db).await;
    let result = async {
        for statement in psql_statements(sql) {
            single.execute_unprepared(&statement).await?;
        }
        Ok(())
    }
    .await;
    single
        .close()
        .await
        .expect("close the psql-like single connection");
    result
}

async fn snapshot(db: &DatabaseConnection) -> Snapshot {
    let mut state = BTreeMap::new();
    for (table, order) in SNAPSHOT_TABLES {
        let query = format!("SELECT to_jsonb(t)::text AS row FROM {table} t ORDER BY {order}");
        let rows = db
            .query_all_raw(Statement::from_string(DbBackend::Postgres, query))
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.try_get("", "row").unwrap())
            .collect();
        state.insert(table, rows);
    }
    state
}

async fn count(db: &DatabaseConnection, table: &str, predicate: &str) -> i64 {
    let query = format!("SELECT COUNT(*)::bigint AS count FROM {table} {predicate}");
    db.query_one_raw(Statement::from_string(DbBackend::Postgres, query))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "count")
        .unwrap()
}

async fn filter_row(db: &DatabaseConnection, id: i64) -> mega_view_filter::Model {
    mega_view_filter::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

async fn filter_definitions(db: &DatabaseConnection) -> Vec<String> {
    db.query_all_raw(Statement::from_string(
        DbBackend::Postgres,
        "SELECT (to_jsonb(t) - 'projected_seq' - 'ready_seq' - 'warming_since')::text AS row \
         FROM mega_view_filter t ORDER BY id"
            .to_owned(),
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| row.try_get("", "row").unwrap())
    .collect()
}

async fn filter_derived_rows(db: &DatabaseConnection, table: &str, filter_pk: i64) -> Vec<String> {
    let query = format!(
        "SELECT to_jsonb(t)::text AS row FROM {table} t WHERE filter_pk = {filter_pk} ORDER BY 1"
    );
    db.query_all_raw(Statement::from_string(DbBackend::Postgres, query))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get("", "row").unwrap())
        .collect()
}

async fn insert_warming_filter(
    db: &DatabaseConnection,
    id: i64,
    spec: &str,
) -> mega_view_filter::Model {
    let canonical = parse_for_registration(spec).unwrap();
    mega_view_filter::ActiveModel {
        id: Set(id),
        filter_id: Set(canonical.filter_id),
        canonical_spec: Set(canonical.canonical_text),
        algo_version: Set(1),
        object_format: Set("sha1".to_owned()),
        src_paths: Set(serde_json::json!([])),
        push_enabled: Set(false),
        projected_seq: Set(0),
        ready_seq: Set(None),
        warming_since: Set(Some(Utc::now().naive_utc())),
        last_access_at: Set(Some(fixed_time())),
        created_at: Set(Utc::now().naive_utc()),
    }
    .insert(db)
    .await
    .unwrap()
}

async fn ops_fixture() -> OpsFixture {
    let temp = tempfile::tempdir().unwrap();
    let db = test_db_connection(temp.path()).await;
    apply_migrations(&db, true).await.unwrap();
    let config = isolated_config(temp.path().join("config"));
    let storage = Storage::new_with_connection(
        Arc::new(config),
        Arc::new(db.clone()),
        mock_object_storage(),
    )
    .await
    .unwrap();

    let history = seed_linear_root_history_with_trees(
        &db,
        HashKind::Sha1,
        (1..=6)
            .map(|seq| {
                root_tree_from_paths(
                    HashKind::Sha1,
                    &[
                        ("README".to_owned(), format!("readme-{seq}").into_bytes()),
                        ("a/x/file".to_owned(), format!("a-{seq}").into_bytes()),
                        ("b/file".to_owned(), format!("b-{seq}").into_bytes()),
                    ],
                )
            })
            .collect(),
    )
    .await;
    assert_eq!(
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap(),
        RootChainOutcome::CaughtUp
    );

    let fourth_root = history[3]
        .trees
        .iter()
        .find(|tree| tree.id == history[3].commit.tree_id)
        .unwrap();
    let missing_tree_id = fourth_root
        .tree_items
        .iter()
        .find(|item| item.name == "a")
        .unwrap()
        .id
        .to_string();
    for earlier in &history[..3] {
        let root = earlier
            .trees
            .iter()
            .find(|tree| tree.id == earlier.commit.tree_id)
            .unwrap();
        assert_ne!(
            root.tree_items
                .iter()
                .find(|item| item.name == "a")
                .unwrap()
                .id
                .to_string(),
            missing_tree_id
        );
    }
    let filter_a = insert_warming_filter(&db, 1, ":/a/x").await;
    let filter_b = insert_warming_filter(&db, 2, ":/b").await;

    mega_tree::Entity::delete_many()
        .filter(mega_tree::Column::TreeId.eq(&missing_tree_id))
        .exec(&db)
        .await
        .unwrap();
    let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
    assert!(matches!(
        service.catch_up(filter_a.id).await.unwrap(),
        CatchUpOutcome::Stopped(stop) if stop.seq == 4
    ));
    assert_eq!(
        service.catch_up(filter_b.id).await.unwrap(),
        CatchUpOutcome::Ready
    );

    mega_view::ActiveModel {
        id: Set(1),
        name: Set("view-a".to_owned()),
        version: Set(1),
        filter_pk: Set(filter_a.id),
        created_by: Set("ops-fixture".to_owned()),
        created_at: Set(Utc::now().naive_utc()),
    }
    .insert(&db)
    .await
    .unwrap();
    mega_view::ActiveModel {
        id: Set(2),
        name: Set("view-b".to_owned()),
        version: Set(1),
        filter_pk: Set(filter_b.id),
        created_by: Set("ops-fixture".to_owned()),
        created_at: Set(Utc::now().naive_utc()),
    }
    .insert(&db)
    .await
    .unwrap();
    for (id, requester) in [(1, "ops-a"), (2, "ops-b")] {
        mega_view_register_log::ActiveModel {
            id: Set(id),
            requester: Set(requester.to_owned()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(&db)
        .await
        .unwrap();
    }

    let unrelated = seed_unrelated_root_history(&db, HashKind::Sha1).await;
    assert!(cas_fixture_main(&db, history.last().unwrap(), &unrelated).await);
    assert!(matches!(
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap(),
        RootChainOutcome::Discontinuous(_)
    ));

    for table in [
        "mega_view_filter",
        "mega_view",
        "mega_view_root_chain",
        "mega_view_root_chain_scan",
        "mega_view_commit_map",
        "mega_view_object",
        "mega_view_object_ref",
        "mega_view_register_log",
    ] {
        assert!(count(&db, table, "").await > 0, "fixture table {table}");
    }
    for filter_pk in [filter_a.id, filter_b.id] {
        assert!(
            count(
                &db,
                "mega_view_commit_map",
                &format!("WHERE filter_pk = {filter_pk}"),
            )
            .await
                > 0
        );
        assert!(
            count(
                &db,
                "mega_view_object_ref",
                &format!("WHERE filter_pk = {filter_pk}"),
            )
            .await
                > 0
        );
        assert!(
            count(
                &db,
                "mega_view_object_ref own",
                &format!(
                    "WHERE own.filter_pk = {filter_pk} AND NOT EXISTS (SELECT 1 FROM mega_view_object_ref other WHERE other.object_id = own.object_id AND other.filter_pk <> {filter_pk})"
                ),
            )
            .await
                > 0,
            "filter {filter_pk} needs an object of its own"
        );
    }
    let a = filter_row(&db, filter_a.id).await;
    let b = filter_row(&db, filter_b.id).await;
    assert_eq!(a.projected_seq, 3);
    assert!(a.warming_since.is_some());
    assert_eq!(a.ready_seq, None);
    assert_eq!(b.ready_seq, Some(6));

    OpsFixture {
        _temp: temp,
        db,
        filter_a: filter_a.id,
        filter_b: filter_b.id,
        filter_a_id: filter_a.filter_id,
    }
}

fn recycle_sql(filter_id: &str) -> String {
    ops_sql("recycle-filter").replace("<filter_id>", filter_id)
}

fn assert_recycled(filter: &mega_view_filter::Model) {
    assert_eq!(filter.projected_seq, 0);
    assert_eq!(filter.ready_seq, None);
    assert_eq!(filter.warming_since, None);
}

#[tokio::test]
async fn ops_blocks_extracted_verbatim() {
    let names = ["recycle-filter", "rebuild-all", "clear-root-chain-scan"];
    let marker_re =
        Regex::new(r"^-- view-ops: (recycle-filter|rebuild-all|clear-root-chain-scan)$").unwrap();
    for name in names {
        let marker = format!("-- view-ops: {name}");
        let valid_fences = OPS_DOC
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|lines| lines[0] == "```sql" && lines[1] == marker)
            .count();
        assert_eq!(
            valid_fences, 1,
            "marker must appear once after a top-level SQL fence"
        );
        assert_eq!(OPS_DOC.lines().filter(|line| **line == marker).count(), 1);
        let block = ops_block(name);
        assert!(block.starts_with(&format!("{marker}\n")));
        assert_eq!(OPS_DOC.matches(&format!("```sql\n{block}```")).count(), 1);
        for line in block.lines() {
            assert!(
                !line.contains(';') || (line.ends_with(';') && line.matches(';').count() == 1),
                "semicolon must appear only at a line end: {line}"
            );
        }
    }
    assert_eq!(
        OPS_DOC
            .lines()
            .filter(|line| marker_re.is_match(line))
            .count(),
        3
    );

    let all = names.map(ops_sql).join("\n");
    let placeholder_re = Regex::new(r"<[A-Za-z_]+>").unwrap();
    let placeholders = placeholder_re
        .find_iter(&all)
        .map(|capture| capture.as_str())
        .collect::<Vec<_>>();
    assert!(!placeholders.is_empty());
    assert!(
        placeholders
            .iter()
            .all(|placeholder| *placeholder == "<filter_id>")
    );
    assert!(
        ops_sql("rebuild-all")
            .matches("<filter_id>")
            .next()
            .is_none()
    );
    assert!(
        ops_sql("clear-root-chain-scan")
            .matches("<filter_id>")
            .next()
            .is_none()
    );

    let expected_deletes = [
        (
            "recycle-filter",
            ["mega_view_commit_map", "mega_view_object_ref"].as_slice(),
        ),
        (
            "rebuild-all",
            [
                "mega_view_root_chain",
                "mega_view_root_chain_scan",
                "mega_view_commit_map",
                "mega_view_object",
                "mega_view_object_ref",
            ]
            .as_slice(),
        ),
        (
            "clear-root-chain-scan",
            ["mega_view_root_chain_scan"].as_slice(),
        ),
    ];
    let allowed = ["BEGIN", "COMMIT", "DELETE FROM", "UPDATE MEGA_VIEW_FILTER"];
    let forbidden_targets = ["mega_view_filter", "mega_view", "mega_view_register_log"];
    for (name, expected) in expected_deletes {
        let sql = ops_sql(name);
        assert!(!sql.to_ascii_lowercase().contains("public."));
        for token in sql.split(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '.'
        }) {
            assert!(
                !token
                    .split('.')
                    .any(|part| part.to_ascii_lowercase().starts_with("mega_"))
                    || !token.contains('.'),
                "schema-qualified view table: {token}"
            );
        }
        let statements = psql_statements(&sql);
        if name != "clear-root-chain-scan" {
            assert_eq!(statements.first().map(String::as_str), Some("BEGIN;"));
            assert_eq!(statements.last().map(String::as_str), Some("COMMIT;"));
            assert!(statements[statements.len() - 2].starts_with("UPDATE mega_view_filter"));
        } else {
            assert_eq!(statements.len(), 1);
        }
        let mut deletes = Vec::new();
        for statement in statements {
            let upper = statement.to_ascii_uppercase();
            assert!(allowed.iter().any(|prefix| upper.starts_with(prefix)));
            assert!(!upper.contains("TRUNCATE"));
            if let Some(target) = upper.strip_prefix("DELETE FROM ") {
                let table = target
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .trim_end_matches(';')
                    .to_ascii_lowercase();
                assert!(!table.contains('.'));
                assert!(!forbidden_targets.contains(&table.as_str()));
                deletes.push(table);
            }
        }
        deletes.sort();
        let mut expected = expected.iter().map(ToString::to_string).collect::<Vec<_>>();
        expected.sort();
        assert_eq!(deletes, expected, "wrong deletion targets for {name}");
    }
}

#[tokio::test]
async fn recycle_filter_sql_resets_target() {
    for target_is_a in [true, false] {
        let fixture = ops_fixture().await;
        let (target_pk, untouched_pk) = if target_is_a {
            (fixture.filter_a, fixture.filter_b)
        } else {
            (fixture.filter_b, fixture.filter_a)
        };
        let target_id = filter_row(&fixture.db, target_pk).await.filter_id;
        let before = snapshot(&fixture.db).await;
        let before_definitions = filter_definitions(&fixture.db).await;
        let untouched = filter_row(&fixture.db, untouched_pk).await;
        let untouched_maps =
            filter_derived_rows(&fixture.db, "mega_view_commit_map", untouched_pk).await;
        let untouched_refs =
            filter_derived_rows(&fixture.db, "mega_view_object_ref", untouched_pk).await;

        run_like_psql(&fixture.db, &recycle_sql(&target_id))
            .await
            .unwrap();

        let after = snapshot(&fixture.db).await;
        assert_recycled(&filter_row(&fixture.db, target_pk).await);
        assert_eq!(filter_row(&fixture.db, untouched_pk).await, untouched);
        assert_eq!(filter_definitions(&fixture.db).await, before_definitions);
        for table in [
            "mega_view",
            "mega_view_root_chain",
            "mega_view_root_chain_scan",
            "mega_view_object",
            "mega_view_register_log",
            "mega_commit",
            "mega_tree",
            "mega_refs",
        ] {
            assert_eq!(
                after[table], before[table],
                "unexpected mutation in {table}"
            );
        }
        for table in ["mega_view_commit_map", "mega_view_object_ref"] {
            assert_eq!(
                count(
                    &fixture.db,
                    table,
                    &format!("WHERE filter_pk = {target_pk}"),
                )
                .await,
                0
            );
        }
        assert_eq!(
            filter_derived_rows(&fixture.db, "mega_view_commit_map", untouched_pk).await,
            untouched_maps
        );
        assert_eq!(
            filter_derived_rows(&fixture.db, "mega_view_object_ref", untouched_pk).await,
            untouched_refs
        );
    }

    let fixture = ops_fixture().await;
    let before = snapshot(&fixture.db).await;
    run_like_psql(&fixture.db, &recycle_sql("not-a-filter-id"))
        .await
        .unwrap();
    assert_eq!(snapshot(&fixture.db).await, before);
}

#[tokio::test]
async fn rebuild_all_sql_clears_derived_state() {
    let fixture = ops_fixture().await;
    let before = snapshot(&fixture.db).await;
    let definitions = filter_definitions(&fixture.db).await;

    run_like_psql(&fixture.db, &ops_sql("rebuild-all"))
        .await
        .unwrap();

    let after = snapshot(&fixture.db).await;
    for table in [
        "mega_view_root_chain",
        "mega_view_root_chain_scan",
        "mega_view_commit_map",
        "mega_view_object",
        "mega_view_object_ref",
    ] {
        assert!(after[table].is_empty(), "expected {table} to be empty");
    }
    for table in [
        "mega_view",
        "mega_view_register_log",
        "mega_commit",
        "mega_tree",
        "mega_refs",
    ] {
        assert_eq!(
            after[table], before[table],
            "unexpected mutation in {table}"
        );
    }
    assert_eq!(filter_definitions(&fixture.db).await, definitions);
    assert_recycled(&filter_row(&fixture.db, fixture.filter_a).await);
    assert_recycled(&filter_row(&fixture.db, fixture.filter_b).await);
}

#[tokio::test]
async fn clear_scan_sql_clears_only_scan() {
    let fixture = ops_fixture().await;
    let before = snapshot(&fixture.db).await;
    run_like_psql(&fixture.db, &ops_sql("clear-root-chain-scan"))
        .await
        .unwrap();
    let after = snapshot(&fixture.db).await;
    assert!(after["mega_view_root_chain_scan"].is_empty());
    for (table, rows) in before {
        if table != "mega_view_root_chain_scan" {
            assert_eq!(after[table], rows, "unexpected mutation in {table}");
        }
    }
}

#[tokio::test]
async fn ops_sql_rolls_back_on_failure() {
    for name in ["recycle-filter", "rebuild-all"] {
        let fixture = ops_fixture().await;
        let before = snapshot(&fixture.db).await;
        fixture
            .db
            .execute_unprepared(
                "ALTER TABLE mega_view_filter RENAME COLUMN warming_since TO hp_ops_missing",
            )
            .await
            .unwrap();
        let sql = if name == "recycle-filter" {
            recycle_sql(&fixture.filter_a_id)
        } else {
            ops_sql(name)
        };
        assert!(run_like_psql(&fixture.db, &sql).await.is_err());
        fixture
            .db
            .execute_unprepared(
                "ALTER TABLE mega_view_filter RENAME COLUMN hp_ops_missing TO warming_since",
            )
            .await
            .unwrap();
        assert_eq!(
            snapshot(&fixture.db).await,
            before,
            "{name} did not roll back"
        );
    }

    let fixture = ops_fixture().await;
    fixture
        .db
        .execute_unprepared(
            "ALTER TABLE mega_view_filter RENAME COLUMN warming_since TO hp_ops_missing",
        )
        .await
        .unwrap();
    let no_transaction = psql_statements(&recycle_sql(&fixture.filter_a_id))
        .into_iter()
        .filter(|statement| statement != "BEGIN;" && statement != "COMMIT;")
        .collect::<Vec<_>>()
        .join("\n");
    assert!(run_like_psql(&fixture.db, &no_transaction).await.is_err());
    fixture
        .db
        .execute_unprepared(
            "ALTER TABLE mega_view_filter RENAME COLUMN hp_ops_missing TO warming_since",
        )
        .await
        .unwrap();
    assert_eq!(
        count(
            &fixture.db,
            "mega_view_commit_map",
            &format!("WHERE filter_pk = {}", fixture.filter_a),
        )
        .await,
        0,
        "the failure test must detect a missing transaction boundary"
    );
}

#[tokio::test]
async fn ops_sql_rerun_is_noop() {
    let recycle = ops_fixture().await;
    let recycle_command = recycle_sql(&recycle.filter_a_id);
    run_like_psql(&recycle.db, &recycle_command).await.unwrap();
    let once = snapshot(&recycle.db).await;
    run_like_psql(&recycle.db, &recycle_command).await.unwrap();
    assert_eq!(snapshot(&recycle.db).await, once);
    let missing = recycle_sql("not-a-filter-id");
    run_like_psql(&recycle.db, &missing).await.unwrap();
    assert_eq!(snapshot(&recycle.db).await, once);

    let rebuild = ops_fixture().await;
    run_like_psql(&rebuild.db, &ops_sql("rebuild-all"))
        .await
        .unwrap();
    let once = snapshot(&rebuild.db).await;
    run_like_psql(&rebuild.db, &ops_sql("rebuild-all"))
        .await
        .unwrap();
    assert_eq!(snapshot(&rebuild.db).await, once);

    let scan = ops_fixture().await;
    run_like_psql(&scan.db, &ops_sql("clear-root-chain-scan"))
        .await
        .unwrap();
    let once = snapshot(&scan.db).await;
    run_like_psql(&scan.db, &ops_sql("clear-root-chain-scan"))
        .await
        .unwrap();
    assert_eq!(snapshot(&scan.db).await, once);
}
