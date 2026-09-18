//! UN-30: the read surface an audit command is given.
//!
//! Two things have to be true before a command can claim it changed nothing.
//! The connection must not migrate on the way in — a report that begins by
//! altering the schema it is about to describe has already changed the answer —
//! and the assembly must not write, which the production one does before any
//! command body runs (`init_monorepo`, the notification
//! worker).
//!
//! The database-level guarantee is what these tests lean on. Code being careful
//! is reviewable and can drift; `default_transaction_read_only=on` covers the
//! paths nobody thought to check, which are the ones worth covering.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

use crate::jupiter::storage::{
    ReadOnlyStorage, init::read_only_database_connection, object_storage::mock_object_storage,
};

fn read_only_db_url(url: &str) -> String {
    crate::jupiter::storage::init::read_only_db_url(url).expect("rewrite url")
}

/// The rewritten URL keeps whatever options it already carried.
///
/// This is not hypothetical tidiness: the test harness isolates every database
/// behind a `search_path` set exactly this way, and a rewrite that dropped it
/// would quietly point the connection at a different schema — and then every
/// "nothing changed" assertion below would be about the wrong schema.
#[test]
fn un30_the_read_only_url_keeps_the_options_it_was_given() {
    let rewritten = read_only_db_url(
        "postgres://u:p@localhost:5432/db?options=-csearch_path%3Dmyschema%2Cpublic",
    );

    let url = url::Url::parse(&rewritten).expect("valid url");
    let options = url
        .query_pairs()
        .find(|(key, _)| key == "options")
        .map(|(_, value)| value.into_owned())
        .expect("options survive");

    assert!(
        options.contains("search_path=myschema,public"),
        "the original option must survive: {options}"
    );
    assert!(
        options.contains("default_transaction_read_only=on"),
        "and read-only must be added: {options}"
    );
    assert!(
        options.find("search_path").unwrap()
            < options.find("default_transaction_read_only").unwrap(),
        "read-only is appended last, because libpq applies -c settings left to \
         right and a URL that tried to turn it off must not win: {options}"
    );
}

#[test]
fn un30_a_url_with_no_options_gains_only_the_read_only_one() {
    let rewritten = read_only_db_url("postgres://u:p@localhost:5432/db");
    let url = url::Url::parse(&rewritten).expect("valid url");
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    assert_eq!(
        pairs,
        vec![(
            "options".to_string(),
            "-cdefault_transaction_read_only=on".to_string()
        )]
    );
}

/// A URL that repeats `options` keeps every occurrence.
///
/// libpq reads the whole thing as one space-separated list, so keeping only the
/// last would drop settings the caller asked for — silently, and only for the
/// read-only path.
#[test]
fn un30_every_options_occurrence_is_kept() {
    let rewritten = read_only_db_url(
        "postgres://u:p@localhost:5432/db?options=-csearch_path%3Da&options=-cstatement_timeout%3D5s",
    );
    let url = url::Url::parse(&rewritten).expect("valid url");
    let options: Vec<String> = url
        .query_pairs()
        .filter(|(k, _)| k == "options")
        .map(|(_, v)| v.into_owned())
        .collect();

    assert_eq!(options.len(), 1, "they are merged into one: {options:?}");
    let merged = &options[0];
    assert!(merged.contains("search_path=a"), "{merged}");
    assert!(merged.contains("statement_timeout=5s"), "{merged}");
    assert!(
        merged.ends_with("-cdefault_transaction_read_only=on"),
        "{merged}"
    );
}

#[test]
fn un30_other_query_parameters_are_preserved() {
    let rewritten = read_only_db_url("postgres://u:p@localhost:5432/db?sslmode=require");
    let url = url::Url::parse(&rewritten).expect("valid url");

    assert_eq!(
        url.query_pairs()
            .find(|(k, _)| k == "sslmode")
            .map(|(_, v)| v.into_owned()),
        Some("require".to_string()),
        "an unrelated parameter must not be lost"
    );
    assert!(url.query_pairs().any(|(k, _)| k == "options"));
}

/// The server refuses a write on this connection.
///
/// Asserted against a real Postgres, because the claim is about what the server
/// does, not about what the URL says. The error naming a read-only transaction
/// is what distinguishes "refused" from "the table happened not to exist".
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn un30_a_write_through_the_read_only_connection_is_refused_by_the_server() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_config = crate::jupiter::tests::test_db_config(temp.path()).await;

    // Prepare a table through a normal connection, so the write below fails for
    // being a write rather than for having nowhere to go.
    let writable = crate::jupiter::storage::init::database_connection(&db_config)
        .await
        .expect("writable connection");
    writable
        .execute_unprepared("CREATE TABLE un30_probe (id integer primary key)")
        .await
        .expect("create the probe table");
    writable
        .execute_unprepared("INSERT INTO un30_probe (id) VALUES (1)")
        .await
        .expect("a writable connection can write");

    let read_only = read_only_database_connection(&db_config)
        .await
        .expect("read-only connection");

    let rows = read_only
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT id FROM un30_probe",
        ))
        .await
        .expect("reads still work");
    assert_eq!(rows.len(), 1, "the read-only connection can read");

    let error = read_only
        .execute_unprepared("INSERT INTO un30_probe (id) VALUES (2)")
        .await
        .expect_err("a write must be refused");
    assert!(
        error.to_string().contains("read-only transaction"),
        "the refusal must come from the server: {error}"
    );

    let error = read_only
        .execute_unprepared("DROP TABLE un30_probe")
        .await
        .expect_err("a schema change must be refused too");
    assert!(
        error.to_string().contains("read-only transaction"),
        "the refusal must come from the server: {error}"
    );

    let rows = writable
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT id FROM un30_probe",
        ))
        .await
        .expect("read back through the writable connection");
    assert_eq!(rows.len(), 1, "neither refused write left anything behind");
}

/// Connecting read-only runs no migration.
///
/// The schema is empty before and empty after — `database_connection` would
/// have filled it, which is the whole difference the card is about.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn un30_the_read_only_connection_runs_no_migration() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_config = crate::jupiter::tests::test_db_config(temp.path()).await;

    let read_only = read_only_database_connection(&db_config)
        .await
        .expect("read-only connection");
    let after_readonly = table_count(&read_only).await;
    assert_eq!(
        after_readonly, 0,
        "a read-only connect must leave the schema as it found it — empty"
    );

    // The control: the production connect is what creates them, so the zero
    // above is a decision rather than a test that could never have failed.
    let writable = crate::jupiter::storage::init::database_connection(&db_config)
        .await
        .expect("writable connection");
    assert!(
        table_count(&writable).await > 0,
        "fixture: the normal connect applies the migrations"
    );
}

/// Count tables in this connection's own schema.
///
/// Test databases are isolated by `search_path` inside one shared database, so
/// an unscoped `information_schema` query would see every other test's tables.
async fn table_count(connection: &sea_orm::DatabaseConnection) -> i64 {
    let row = connection
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM information_schema.tables \
             WHERE table_schema = current_schema()",
        ))
        .await
        .expect("count tables")
        .expect("one row");
    row.try_get::<i64>("", "n").expect("count")
}

/// The read facade does not write through production storage assembly side
/// effects that used to seed UI menu rows.
///
/// Historically `Storage::new_with_connection` wrote default sidebars on the
/// way in; that seed and the table are gone (RM-SB). This test proves the
/// read facade and production assembly do not recreate the leftover menu table.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn un30_building_the_read_facade_writes_no_sidebar() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_config = crate::jupiter::tests::test_db_config(temp.path()).await;
    let writable = Arc::new(
        crate::jupiter::storage::init::database_connection(&db_config)
            .await
            .expect("writable connection"),
    );
    let config = Arc::new(crate::config::testing::isolated_config(
        temp.path().join("config"),
    ));

    assert!(
        !sidebar_table_exists(&writable).await,
        "fixture: a fresh schema has no leftover menu table"
    );

    let read_only = Arc::new(
        read_only_database_connection(&db_config)
            .await
            .expect("read-only connection"),
    );
    let facade = ReadOnlyStorage::new(config.clone(), read_only, mock_object_storage());
    assert!(
        !sidebar_table_exists(&writable).await,
        "assembling the read facade must not recreate the leftover menu table"
    );
    // The facade is usable for reading afterwards, which is the point of it
    // existing at all.
    assert!(
        facade
            .mono_storage()
            .get_main_ref("/")
            .await
            .expect("a read through the facade must work")
            .is_none(),
        "fixture: a fresh schema has no root ref"
    );

    crate::jupiter::storage::Storage::new_with_connection(
        config,
        writable.clone(),
        mock_object_storage(),
    )
    .await
    .expect("full assembly");
    assert!(
        !sidebar_table_exists(&writable).await,
        "production assembly must not recreate the leftover menu table"
    );
}

async fn sidebar_table_exists(connection: &sea_orm::DatabaseConnection) -> bool {
    let table = format!("{}_{}", "dynamic", "sidebar");
    let row = connection
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT to_regclass('{table}')::text AS table_name"),
        ))
        .await
        .expect("query catalog")
        .expect("one row");
    row.try_get::<Option<String>>("", "table_name")
        .expect("table_name")
        .is_some()
}

/// A config with no vault-backed credentials needs no vault.
///
/// Opening one anyway would make an audit fail on a deployment that has no
/// vault, for the sake of reading nothing out of it.
#[test]
fn un30_a_plain_object_storage_config_needs_no_vault() {
    let config = crate::config::testing::isolated_config(std::path::PathBuf::from("/tmp/un30"));
    assert!(
        !super::object_storage_needs_vault(&config.object_storage),
        "a local object store has no credentials to resolve"
    );
}
