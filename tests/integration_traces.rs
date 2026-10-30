#[path = "common/traces_db.rs"]
mod traces_db;

use std::path::PathBuf;

use chrono::Utc;
use mega2_core::{
    config::{
        Config,
        loader::{ConfigInput, ConfigLoader},
        reload::ConfigHandle,
    },
    trace_checkpoint, trace_credential_binding, trace_fragment, trace_ingest_attempt,
    trace_principal, trace_ref_update, trace_resource, trace_stream, trace_target,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter, Set,
    Statement,
};
use uuid::Uuid;

struct CredentialCliFixture {
    root: tempfile::TempDir,
    config: PathBuf,
    database_url: String,
}

impl CredentialCliFixture {
    async fn new(db: &sea_orm::DatabaseConnection) -> Self {
        let database: String = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT current_database() AS name",
            ))
            .await
            .expect("database name")
            .expect("name row")
            .try_get("", "name")
            .expect("name");
        let admin = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
            "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
        });
        let mut url = url::Url::parse(&admin).expect("test database URL");
        url.set_path(&database);
        let root = tempfile::tempdir().expect("credential fixture");
        let config = root.path().join("operator.toml");
        let mut text = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
        )
        .expect("config template");
        assert!(
            !text
                .lines()
                .any(|line| line.trim() == "[[git.push_tokens]]")
        );
        for name in ["first", "second", "third", "access-token:31"] {
            text.push_str(&format!("\n[[git.push_tokens]]\nname = \"{name}\"\ntoken = \"synthetic-config-secret-{name}\"\npaths = [\"/project\"]\n"));
        }
        std::fs::write(&config, text).expect("named config");
        Self {
            root,
            config,
            database_url: url.to_string(),
        }
    }

    fn command(&self, args: &[&str], named: bool) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mega2"));
        command
            .current_dir(self.root.path())
            .env_remove("MEGA_CONFIG")
            .env_remove("MEGA_PROFILE")
            .env_remove("MEGA_GIT__PUSH_TOKENS")
            .env("MEGA_DATABASE__DB_TYPE", "postgres")
            .env("MEGA_DATABASE__DB_URL", &self.database_url)
            .env("MEGA_DATABASE__MIN_CONNECTION", "0")
            .env("MEGA_DATABASE__MAX_CONNECTION", "2")
            .env("MEGA_BASE_DIR", self.root.path().join("base"))
            .env("MEGA_CACHE_DIR", self.root.path().join("cache"))
            .env("MEGA_LOG__PRINT_STD", "true");
        if named {
            command.arg("--config").arg(&self.config);
        }
        command.args(args);
        command
    }

    fn spawn(&self, args: &[&str], named: bool) -> CredentialProcess {
        CredentialProcess::spawn(self.command(args, named), self.root.path())
    }

    fn run(&self, args: &[&str], named: bool) -> std::process::Output {
        self.spawn(args, named).finish()
    }
}

struct CredentialProcess {
    child: std::process::Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl CredentialProcess {
    fn spawn(mut command: std::process::Command, root: &std::path::Path) -> Self {
        let id = Uuid::new_v4();
        let stdout = root.join(format!("{id}.stdout"));
        let stderr = root.join(format!("{id}.stderr"));
        command
            .stdout(std::fs::File::create(&stdout).expect("stdout file"))
            .stderr(std::fs::File::create(&stderr).expect("stderr file"));
        Self {
            child: command.spawn().expect("credential CLI"),
            stdout,
            stderr,
        }
    }

    fn finish(mut self) -> std::process::Output {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll credential CLI") {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "credential CLI did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        std::process::Output {
            status,
            stdout: std::fs::read(&self.stdout).expect("CLI stdout"),
            stderr: std::fs::read(&self.stderr).expect("CLI stderr"),
        }
    }
}

impl Drop for CredentialProcess {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn credential_output(output: &std::process::Output, success: bool) -> String {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.success(), success, "{text}");
    for secret in [
        "synthetic-config-secret-",
        "synthetic-access-secret",
        "synthetic-database-secret",
        "synthetic-private-key-value",
    ] {
        assert!(
            !text.contains(secret),
            "credential diagnostic leaked a synthetic secret"
        );
    }
    text
}

fn credential_success(output: &std::process::Output, action: &str) -> (String, String, String) {
    credential_output(output, true);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| line.starts_with(&format!("{action} ")))
        .expect("credential success row");
    let fields: Vec<_> = line.split_whitespace().collect();
    assert_eq!(fields.len(), 4);
    for id in &fields[1..3] {
        assert_eq!(Uuid::parse_str(id).expect("UUID").to_string(), *id);
    }
    (
        fields[1].to_owned(),
        fields[2].to_owned(),
        fields[3].to_owned(),
    )
}

async fn credential_row(
    db: &sea_orm::DatabaseConnection,
    id: &str,
) -> trace_credential_binding::Model {
    trace_credential_binding::Entity::find_by_id(id)
        .one(db)
        .await
        .expect("binding query")
        .expect("binding retained")
}

async fn credential_counts(db: &sea_orm::DatabaseConnection) -> (i64, i64) {
    let row = db.query_one_raw(Statement::from_string(DatabaseBackend::Postgres,
        "SELECT (SELECT count(*) FROM trace_principal)::bigint AS principals, (SELECT count(*) FROM trace_credential_binding)::bigint AS bindings"
    )).await.expect("identity counts").expect("counts row");
    (
        row.try_get("", "principals").expect("principals"),
        row.try_get("", "bindings").expect("bindings"),
    )
}

async fn credential_sources(db: &sea_orm::DatabaseConnection) {
    mega2_core::access_token::ActiveModel {
        id: Set(31),
        username: Set("same-synthetic-account".to_owned()),
        token: Set("synthetic-access-secret".to_owned()),
        created_at: Set(Utc::now().naive_utc()),
    }
    .insert(db)
    .await
    .expect("access-token source");
    mega2_core::ssh_keys::ActiveModel {
        id: Set(31),
        username: Set("same-synthetic-account".to_owned()),
        title: Set("fixture".to_owned()),
        ssh_key: Set("synthetic-public-key".to_owned()),
        finger: Set("fixture-fingerprint".to_owned()),
        created_at: Set(Utc::now().naive_utc()),
    }
    .insert(db)
    .await
    .expect("SSH source");
}

#[test]
fn credential_binding_cli_help_exposes_only_lifecycle_inputs() {
    let root = tempfile::tempdir().expect("help fixture");
    for operation in [None, Some("bind"), Some("rotate"), Some("retire")] {
        let mut args = vec!["traces", "credentials"];
        if let Some(operation) = operation {
            args.push(operation);
        }
        args.push("--help");
        let output = rebind_output(root.path(), &args, &[]);
        let text = credential_output(&output, true);
        match operation {
            None => {
                for child in ["bind", "rotate", "retire"] {
                    assert!(text.contains(child));
                }
            }
            Some("bind") => {
                assert!(text.contains("--principal"));
                assert!(text.contains("--namespace"));
                assert!(text.contains("--credential"));
            }
            Some("rotate") => {
                assert!(text.contains("--binding"));
                assert!(text.contains("--namespace"));
                assert!(text.contains("--credential"));
            }
            _ => {
                assert!(text.contains("--binding"));
                assert!(!text.contains("--credential"));
            }
        }
        assert!(!text.contains("--secret"));
    }
}

#[tokio::test]
async fn credential_binding_cli_requires_named_existing_configuration() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let fixture = CredentialCliFixture::new(&db).await;
    let principal = Uuid::new_v4().to_string();
    let args = [
        "traces",
        "credentials",
        "bind",
        "--principal",
        &principal,
        "--namespace",
        "static_push_token",
        "--credential",
        "first",
    ];
    let unnamed = fixture.run(&args, false);
    credential_output(&unnamed, false);
    assert!(!fixture.root.path().join("base/etc/config.toml").exists());
    let discovered = fixture.root.path().join("config");
    std::fs::create_dir(&discovered).expect("discovered config directory");
    std::fs::copy(&fixture.config, discovered.join("config.toml")).expect("discovered config");
    assert!(credential_output(&fixture.run(&args, false), false).contains("was not named"));
    let mut missing = vec!["--config", "missing.toml"];
    missing.extend(args);
    credential_output(&fixture.run(&missing, false), false);
    assert_eq!(credential_counts(&db).await, (0, 0));
    let mut command = fixture.command(&args, false);
    command.env("MEGA_CONFIG", &fixture.config);
    let output = CredentialProcess::spawn(command, fixture.root.path()).finish();
    let (_, owner, state) = credential_success(&output, "bind");
    assert_eq!(owner, principal);
    assert_eq!(state, "active");
    assert_eq!(credential_counts(&db).await, (1, 1));
    db.close().await.expect("close database");
}

#[tokio::test]
async fn credential_binding_cli_rejects_old_and_unknown_schema_without_writes() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let fixture = CredentialCliFixture::new(&db).await;
    let principal = Uuid::new_v4().to_string();
    let args = [
        "traces",
        "credentials",
        "bind",
        "--principal",
        &principal,
        "--namespace",
        "static_push_token",
        "--credential",
        "first",
    ];
    let removed = db.query_one_raw(Statement::from_string(DatabaseBackend::Postgres,
        "DELETE FROM seaql_migrations WHERE version = (SELECT version FROM seaql_migrations ORDER BY version DESC LIMIT 1) RETURNING version, applied_at"
    )).await.expect("old ledger fixture").expect("removed row");
    let version: String = removed.try_get("", "version").expect("version");
    let applied_at: i64 = removed
        .try_get("", "applied_at")
        .expect("applied timestamp");
    assert!(credential_output(&fixture.run(&args, true), false).contains("schema is behind"));
    assert_eq!(credential_counts(&db).await, (0, 0));
    let still_absent = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT version FROM seaql_migrations WHERE version = $1",
            [version.clone().into()],
        ))
        .await
        .expect("unchanged old ledger");
    assert!(still_absent.is_none());
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "INSERT INTO seaql_migrations(version, applied_at) VALUES ($1, $2)",
        [version.into(), applied_at.into()],
    ))
    .await
    .expect("restore fixture ledger");
    db.execute_unprepared("INSERT INTO seaql_migrations(version, applied_at) VALUES ('m99999999_synthetic-private-key-value', 0)").await.expect("unknown migration fixture");
    assert!(credential_output(&fixture.run(&args, true), false).contains("cannot confirm"));
    assert_eq!(credential_counts(&db).await, (0, 0));
    assert!(db.query_one_raw(Statement::from_string(DatabaseBackend::Postgres,
        "SELECT version FROM seaql_migrations WHERE version = 'm99999999_synthetic-private-key-value'"
    )).await.expect("unknown ledger retained").is_some());
    db.close().await.expect("close database");
}

#[tokio::test]
async fn credential_binding_cli_accepts_real_sources_with_explicit_principals() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    credential_sources(&db).await;
    let fixture = CredentialCliFixture::new(&db).await;
    for (namespace, key) in [
        ("static_push_token", "access-token:31"),
        ("mono_account", "access-token:31"),
        ("mono_account", "ssh-key:31"),
    ] {
        let principal = Uuid::new_v4();
        let form = principal.urn().to_string();
        let args = [
            "traces",
            "credentials",
            "bind",
            "--principal",
            &form,
            "--namespace",
            namespace,
            "--credential",
            key,
        ];
        let (id, owner, state) = credential_success(&fixture.run(&args, true), "bind");
        assert_eq!(owner, principal.to_string());
        assert_eq!(state, "active");
        let row = credential_row(&db, &id).await;
        assert_eq!(row.principal_id, owner);
        assert_eq!(row.namespace, namespace);
        assert_eq!(row.credential_id, key);
        assert!(row.retired_at.is_none());
        let repeated = credential_success(&fixture.run(&args, true), "bind");
        assert_eq!(repeated.0, id);
        assert_eq!(credential_row(&db, &id).await, row);
    }
    assert_eq!(credential_counts(&db).await, (3, 3));
    db.close().await.expect("close database");
}

#[tokio::test]
async fn credential_binding_cli_invalid_credentials_and_config_do_not_leak_or_write() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let fixture = CredentialCliFixture::new(&db).await;
    let principal = Uuid::new_v4().to_string();
    for (owner, namespace, key) in [
        ("synthetic-private-key-value", "static_push_token", "first"),
        (principal.as_str(), "unknown", "synthetic-private-key-value"),
        (
            principal.as_str(),
            "static_push_token",
            "synthetic-config-secret-first",
        ),
        (principal.as_str(), "mono_account", "access-token:31"),
        (principal.as_str(), "mono_account", "ssh-key:31"),
        (principal.as_str(), "mono_account", "access-token:01"),
        (
            principal.as_str(),
            "mono_account",
            "access-token:9223372036854775808",
        ),
        (
            principal.as_str(),
            "mono_account",
            "username:synthetic-private-key-value",
        ),
    ] {
        credential_output(
            &fixture.run(
                &[
                    "traces",
                    "credentials",
                    "bind",
                    "--principal",
                    owner,
                    "--namespace",
                    namespace,
                    "--credential",
                    key,
                ],
                true,
            ),
            false,
        );
        assert_eq!(credential_counts(&db).await, (0, 0));
    }
    let mut text = std::fs::read_to_string(&fixture.config).expect("fixture config");
    text.push_str(
        "\n[[git.push_tokens]]\nname = \"first\"\ntoken = \"synthetic-private-key-value\"\n",
    );
    std::fs::write(&fixture.config, text).expect("invalid duplicate token config");
    assert!(
        credential_output(
            &fixture.run(
                &[
                    "traces",
                    "credentials",
                    "bind",
                    "--principal",
                    &principal,
                    "--namespace",
                    "static_push_token",
                    "--credential",
                    "first"
                ],
                true
            ),
            false
        )
        .contains("invalid maintenance configuration")
    );
    assert_eq!(credential_counts(&db).await, (0, 0));
    db.close().await.expect("close database");
}

#[tokio::test]
async fn credential_binding_cli_rotation_conflicts_and_retirement_keep_history() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let fixture = CredentialCliFixture::new(&db).await;
    let principal = Uuid::new_v4().to_string();
    let (old_id, _, _) = credential_success(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "bind",
                "--principal",
                &principal,
                "--namespace",
                "static_push_token",
                "--credential",
                "first",
            ],
            true,
        ),
        "bind",
    );
    let old = credential_row(&db, &old_id).await;
    let losing_principal = Uuid::new_v4().to_string();
    credential_output(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "bind",
                "--principal",
                &losing_principal,
                "--namespace",
                "static_push_token",
                "--credential",
                "first",
            ],
            true,
        ),
        false,
    );
    assert!(
        trace_principal::Entity::find_by_id(losing_principal)
            .one(&db)
            .await
            .expect("losing principal")
            .is_none()
    );
    assert_eq!(credential_counts(&db).await, (1, 1));
    let (destination, _, _) = credential_success(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "bind",
                "--principal",
                &principal,
                "--namespace",
                "static_push_token",
                "--credential",
                "second",
            ],
            true,
        ),
        "bind",
    );
    for key in ["first", "second"] {
        credential_output(
            &fixture.run(
                &[
                    "traces",
                    "credentials",
                    "rotate",
                    "--binding",
                    &old_id,
                    "--namespace",
                    "static_push_token",
                    "--credential",
                    key,
                ],
                true,
            ),
            false,
        );
        assert_eq!(credential_row(&db, &old_id).await, old);
    }
    credential_success(
        &fixture.run(
            &["traces", "credentials", "retire", "--binding", &destination],
            true,
        ),
        "retire",
    );
    let (replacement, owner, state) = credential_success(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "rotate",
                "--binding",
                &old_id,
                "--namespace",
                "static_push_token",
                "--credential",
                "third",
            ],
            true,
        ),
        "rotate",
    );
    assert_eq!(owner, principal);
    assert_eq!(state, "active");
    assert_ne!(replacement, old_id);
    let retired_old = credential_row(&db, &old_id).await;
    assert!(retired_old.retired_at.is_some());
    assert_eq!(retired_old.credential_id, old.credential_id);
    assert_eq!(retired_old.created_at, old.created_at);
    assert_eq!(
        credential_row(&db, &replacement).await.principal_id,
        principal
    );
    credential_output(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "rotate",
                "--binding",
                &old_id,
                "--namespace",
                "static_push_token",
                "--credential",
                "second",
            ],
            true,
        ),
        false,
    );
    let missing = Uuid::new_v4().to_string();
    credential_output(
        &fixture.run(
            &["traces", "credentials", "retire", "--binding", &missing],
            true,
        ),
        false,
    );
    credential_output(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "rotate",
                "--binding",
                &missing,
                "--namespace",
                "static_push_token",
                "--credential",
                "second",
            ],
            true,
        ),
        false,
    );
    let (_, _, state) = credential_success(
        &fixture.run(
            &["traces", "credentials", "retire", "--binding", &replacement],
            true,
        ),
        "retire",
    );
    assert_eq!(state, "retired");
    let retired = credential_row(&db, &replacement).await;
    credential_success(
        &fixture.run(
            &["traces", "credentials", "retire", "--binding", &replacement],
            true,
        ),
        "retire",
    );
    assert_eq!(credential_row(&db, &replacement).await, retired);
    assert_eq!(credential_row(&db, &old_id).await, retired_old);
    assert!(
        trace_credential_binding::Entity::find()
            .filter(trace_credential_binding::Column::RetiredAt.is_null())
            .all(&db)
            .await
            .expect("active bindings")
            .is_empty()
    );
    assert_eq!(credential_counts(&db).await, (1, 3));
    db.close().await.expect("close database");
}

#[tokio::test]
async fn credential_binding_cli_database_failure_rolls_back_rotation_and_redacts_error() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let fixture = CredentialCliFixture::new(&db).await;
    let principal = Uuid::new_v4().to_string();
    let (id, _, _) = credential_success(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "bind",
                "--principal",
                &principal,
                "--namespace",
                "static_push_token",
                "--credential",
                "first",
            ],
            true,
        ),
        "bind",
    );
    let old = credential_row(&db, &id).await;
    db.execute_unprepared("CREATE FUNCTION reject_fixture_retire() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic-database-secret'; END $$; CREATE TRIGGER reject_fixture_retire BEFORE UPDATE ON trace_credential_binding FOR EACH ROW EXECUTE FUNCTION reject_fixture_retire()")
        .await.expect("transaction failure fixture");
    let text = credential_output(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "rotate",
                "--binding",
                &id,
                "--namespace",
                "static_push_token",
                "--credential",
                "second",
            ],
            true,
        ),
        false,
    );
    assert!(text.contains("credential storage failed"));
    assert_eq!(credential_row(&db, &id).await, old);
    assert_eq!(credential_counts(&db).await, (1, 1));
    db.close().await.expect("close database");
}

async fn credential_barrier_pid(txn: &sea_orm::DatabaseTransaction) -> i32 {
    txn.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT pg_backend_pid() AS pid",
    ))
    .await
    .expect("barrier PID")
    .expect("PID row")
    .try_get("", "pid")
    .expect("PID")
}

async fn credential_wait_for_cli_lock_waiters(db: &sea_orm::DatabaseConnection, blocker: i32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let count: i64 = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "WITH RECURSIVE waits(pid, blocker) AS ( \
             SELECT pid, unnest(pg_blocking_pids(pid)) FROM pg_stat_activity \
             WHERE datname = current_database() UNION \
             SELECT waits.pid, unnest(pg_blocking_pids(waits.blocker)) FROM waits) \
             SELECT count(DISTINCT pid)::bigint AS n FROM waits WHERE blocker = $1",
                [blocker.into()],
            ))
            .await
            .expect("CLI wait graph")
            .expect("wait count")
            .try_get("", "n")
            .expect("wait count");
        if count >= 2 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "both CLI transactions must reach the lock barrier"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn credential_concurrent_bind_case(same_principal: bool) {
    use sea_orm::TransactionTrait;

    let (_traces, db) = traces_db::TracesDb::migrated().await;
    credential_sources(&db).await;
    let fixture = CredentialCliFixture::new(&db).await;
    let first = Uuid::new_v4().to_string();
    let second = if same_principal {
        first.clone()
    } else {
        Uuid::new_v4().to_string()
    };
    let barrier = db.begin().await.expect("source barrier");
    assert!(
        barrier
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT id FROM access_token WHERE id = 31 FOR UPDATE"
            ))
            .await
            .expect("hold source")
            .is_some()
    );
    let pid = credential_barrier_pid(&barrier).await;
    let left = fixture.spawn(
        &[
            "traces",
            "credentials",
            "bind",
            "--principal",
            &first,
            "--namespace",
            "mono_account",
            "--credential",
            "access-token:31",
        ],
        true,
    );
    let right = fixture.spawn(
        &[
            "traces",
            "credentials",
            "bind",
            "--principal",
            &second,
            "--namespace",
            "mono_account",
            "--credential",
            "access-token:31",
        ],
        true,
    );
    assert_ne!(left.child.id(), right.child.id());
    credential_wait_for_cli_lock_waiters(&db, pid).await;
    barrier.rollback().await.expect("release source barrier");
    let left = left.finish();
    let right = right.finish();
    if same_principal {
        let left = credential_success(&left, "bind");
        let right = credential_success(&right, "bind");
        assert_eq!(left, right);
        assert_eq!(left.1, first);
        assert_eq!(credential_row(&db, &left.0).await.principal_id, first);
    } else {
        assert_ne!(left.status.success(), right.status.success());
        let (success, failure, loser) = if left.status.success() {
            (&left, &right, &second)
        } else {
            (&right, &left, &first)
        };
        let (_, winner, state) = credential_success(success, "bind");
        assert_eq!(state, "active");
        assert_ne!(winner, *loser);
        assert!(credential_output(failure, false).contains("active binding"));
        assert!(
            trace_principal::Entity::find_by_id(loser.as_str())
                .one(&db)
                .await
                .expect("losing principal")
                .is_none()
        );
    }
    assert_eq!(credential_counts(&db).await, (1, 1));
    db.close().await.expect("close database");
}

#[tokio::test]
async fn credential_binding_cli_concurrent_new_principal_bind_is_idempotent() {
    credential_concurrent_bind_case(true).await;
}

#[tokio::test]
async fn credential_binding_cli_concurrent_owner_conflict_rolls_back_loser() {
    credential_concurrent_bind_case(false).await;
}

#[tokio::test]
async fn credential_binding_cli_concurrent_rotations_commit_only_one_replacement() {
    use sea_orm::TransactionTrait;

    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let fixture = CredentialCliFixture::new(&db).await;
    let principal = Uuid::new_v4().to_string();
    let (id, _, _) = credential_success(
        &fixture.run(
            &[
                "traces",
                "credentials",
                "bind",
                "--principal",
                &principal,
                "--namespace",
                "static_push_token",
                "--credential",
                "first",
            ],
            true,
        ),
        "bind",
    );
    let barrier = db.begin().await.expect("binding barrier");
    assert!(
        barrier
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT id FROM trace_credential_binding WHERE id = $1 FOR UPDATE",
                [id.clone().into()],
            ))
            .await
            .expect("hold binding")
            .is_some()
    );
    let pid = credential_barrier_pid(&barrier).await;
    let left = fixture.spawn(
        &[
            "traces",
            "credentials",
            "rotate",
            "--binding",
            &id,
            "--namespace",
            "static_push_token",
            "--credential",
            "second",
        ],
        true,
    );
    let right = fixture.spawn(
        &[
            "traces",
            "credentials",
            "rotate",
            "--binding",
            &id,
            "--namespace",
            "static_push_token",
            "--credential",
            "third",
        ],
        true,
    );
    assert_ne!(left.child.id(), right.child.id());
    credential_wait_for_cli_lock_waiters(&db, pid).await;
    barrier.rollback().await.expect("release binding barrier");
    let left = left.finish();
    let right = right.finish();
    assert_ne!(left.status.success(), right.status.success());
    let (success, failure) = if left.status.success() {
        (&left, &right)
    } else {
        (&right, &left)
    };
    let (replacement, owner, state) = credential_success(success, "rotate");
    assert_eq!(owner, principal);
    assert_eq!(state, "active");
    assert_ne!(replacement, id);
    assert!(credential_output(failure, false).contains("already been retired"));
    assert!(credential_row(&db, &id).await.retired_at.is_some());
    let replacement = credential_row(&db, &replacement).await;
    assert_eq!(replacement.principal_id, principal);
    assert!(replacement.retired_at.is_none());
    assert_eq!(credential_counts(&db).await, (1, 2));
    db.close().await.expect("close database");
}

async fn table_exists(db: &sea_orm::DatabaseConnection, table: &str) -> bool {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT to_regclass('public.{table}')::text AS table_name"),
        ))
        .await
        .expect("catalog query")
        .expect("catalog row");
    row.try_get::<Option<String>>("", "table_name")
        .expect("table_name")
        .is_some()
}

#[tokio::test]
async fn schema_identity_principal_persists() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let id = Uuid::new_v4().to_string();
    trace_principal::ActiveModel {
        id: Set(id.clone()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("insert principal");
    let loaded = trace_principal::Entity::find_by_id(id.clone())
        .one(&db)
        .await
        .expect("load principal")
        .expect("principal row");
    assert_eq!(loaded.id, id);
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_identity_namespaces_are_independent() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let token_owner = Uuid::new_v4().to_string();
    let account_owner = Uuid::new_v4().to_string();
    for id in [&token_owner, &account_owner] {
        trace_principal::ActiveModel {
            id: Set(id.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("insert principal");
    }
    let credential_id = "cred-synthetic-beta";
    for (principal_id, namespace) in [
        (token_owner.as_str(), "static_push_token"),
        (account_owner.as_str(), "mono_account"),
    ] {
        trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.to_owned()),
            namespace: Set(namespace.to_owned()),
            credential_id: Set(credential_id.to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
            retired_at: Set(None),
        }
        .insert(&db)
        .await
        .expect("insert binding");
    }
    let rows = trace_credential_binding::Entity::find()
        .filter(trace_credential_binding::Column::CredentialId.eq(credential_id))
        .all(&db)
        .await
        .expect("load bindings");
    assert_eq!(rows.len(), 2);
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_identity_path_rebuild_keeps_code_and_capture_tables() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let path = "/team/rebuild";
    let first = trace_target::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        target_kind: Set("native".to_owned()),
        project_path: Set(path.to_owned()),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("first target");
    let mut inactive: trace_target::ActiveModel =
        trace_target::Entity::find_by_id(first.id.clone())
            .one(&db)
            .await
            .expect("load first")
            .expect("first row")
            .into();
    inactive.active = Set(false);
    inactive.update(&db).await.expect("deactivate");
    let rebuilt = trace_target::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        target_kind: Set("native".to_owned()),
        project_path: Set(path.to_owned()),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("rebuilt target");
    assert_ne!(first.id, rebuilt.id);
    assert!(table_exists(&db, "git_repo").await);
    assert!(table_exists(&db, "agent_capture_session").await);
    assert!(table_exists(&db, "trace_principal").await);
    db.close().await.expect("close traces database");
}

async fn published_stream(db: &sea_orm::DatabaseConnection) -> String {
    let principal_id = Uuid::new_v4().to_string();
    let target_id = Uuid::new_v4().to_string();
    trace_principal::ActiveModel {
        id: Set(principal_id.clone()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(db)
    .await
    .expect("principal");
    trace_target::ActiveModel {
        id: Set(target_id.clone()),
        target_kind: Set("native".to_owned()),
        project_path: Set(format!("/pub/{target_id}")),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(db)
    .await
    .expect("target");
    trace_stream::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        principal_id: Set(principal_id),
        target_id: Set(target_id),
        source_repo_id: Set("source-opaque".to_owned()),
        epoch_id: Set(Uuid::new_v4().to_string()),
        hash_kind: Set("sha1".to_owned()),
        tip_oid: Set(None),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(db)
    .await
    .expect("stream")
    .id
}

#[tokio::test]
async fn schema_publication_stream_natural_key() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let principal_id = Uuid::new_v4().to_string();
    let target_id = Uuid::new_v4().to_string();
    let epoch_id = Uuid::new_v4().to_string();
    trace_principal::ActiveModel {
        id: Set(principal_id.clone()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("principal");
    trace_target::ActiveModel {
        id: Set(target_id.clone()),
        target_kind: Set("native".to_owned()),
        project_path: Set(format!("/pub/{target_id}")),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("target");
    let first = trace_stream::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        principal_id: Set(principal_id.clone()),
        target_id: Set(target_id.clone()),
        source_repo_id: Set("source-opaque".to_owned()),
        epoch_id: Set(epoch_id.clone()),
        hash_kind: Set("sha1".to_owned()),
        tip_oid: Set(None),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("stream");
    let duplicate = trace_stream::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        principal_id: Set(principal_id),
        target_id: Set(target_id),
        source_repo_id: Set("source-opaque".to_owned()),
        epoch_id: Set(epoch_id),
        hash_kind: Set("sha1".to_owned()),
        tip_oid: Set(None),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await;
    assert!(duplicate.is_err(), "natural key is unique");
    let changed = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE trace_stream SET hash_kind = 'sha256' WHERE id = $1",
            [first.id.into()],
        ))
        .await;
    assert!(changed.is_err(), "hash_kind is immutable");
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_publication_ref_journal_unique() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let stream_id = published_stream(&db).await;
    trace_ingest_attempt::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        idempotency_key: Set("attempt-1".to_owned()),
        state: Set("published".to_owned()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("attempt");
    trace_ref_update::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        ref_sequence: Set(1),
        idempotency_key: Set("journal-1".to_owned()),
        old_tip: Set(None),
        new_tip: Set("aabb".to_owned()),
        state: Set("pending".to_owned()),
        claim_until: Set(None),
        attempt_count: Set(0),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("journal");
    let same_sequence = trace_ref_update::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id),
        ref_sequence: Set(1),
        idempotency_key: Set("journal-2".to_owned()),
        old_tip: Set(None),
        new_tip: Set("ccdd".to_owned()),
        state: Set("pending".to_owned()),
        claim_until: Set(None),
        attempt_count: Set(0),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await;
    assert!(same_sequence.is_err(), "ref_sequence is unique");
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_publication_keeps_code_and_capture_rows() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    db.execute_unprepared(
        "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) \
         VALUES (91004, '/code/it', 'it', now(), now())",
    )
    .await
    .expect("code row");
    db.execute_unprepared(
        "INSERT INTO agent_capture_session (
            deployment_id, tenant_id, repo_id, producer_id, session_kind,
            client_session_id, completeness, created_at, updated_at
         ) VALUES (
            'dep', 'tenant', 'repo', 'producer', 'external_capture',
            'client-mtr03-it', 'complete', now(), now()
         )",
    )
    .await
    .expect("capture row");
    assert!(table_exists(&db, "trace_stream").await);
    assert!(table_exists(&db, "trace_ref_update").await);
    let code = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM git_repo WHERE id = 91004".to_owned(),
        ))
        .await
        .expect("count code")
        .expect("code row");
    let capture = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM agent_capture_session WHERE client_session_id = 'client-mtr03-it'".to_owned(),
        ))
        .await
        .expect("count capture")
        .expect("capture row");
    assert_eq!(code.try_get::<i64>("", "n").expect("n"), 1);
    assert_eq!(capture.try_get::<i64>("", "n").expect("n"), 1);
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_projection_checkpoint_resource_and_fragment() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let stream_id = published_stream(&db).await;
    let checkpoint = trace_checkpoint::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        checkpoint_key: Set("ckpt-public".to_owned()),
        original_tree: Set("tree-public".to_owned()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("checkpoint");
    let duplicate = trace_checkpoint::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        checkpoint_key: Set("ckpt-public".to_owned()),
        original_tree: Set("tree-other".to_owned()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await;
    assert!(duplicate.is_err(), "checkpoint key is unique in the stream");
    let changed = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE trace_checkpoint SET original_tree = 'tree-mutated' WHERE id = $1",
            [checkpoint.id.clone().into()],
        ))
        .await;
    assert!(changed.is_err(), "original_tree is immutable");
    let resource = trace_resource::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        checkpoint_id: Set(checkpoint.id.clone()),
        ordinal: Set(4),
        byte_start: Set(32),
        byte_end: Set(64),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("resource");
    let fragment = trace_fragment::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        content_digest: Set("digest-public".to_owned()),
        index_version: Set(1),
        body: Set("Hello".to_owned()),
        ascii_fold: Set("hello".to_owned()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("fragment");
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "INSERT INTO trace_fragment_membership (
            id, fragment_id, stream_id, checkpoint_id, resource_id, created_at
         ) VALUES ($1, $2, $3, $4, $5, now())",
        [
            Uuid::new_v4().to_string().into(),
            fragment.id.clone().into(),
            stream_id.into(),
            checkpoint.id.into(),
            resource.id.clone().into(),
        ],
    ))
    .await
    .expect("membership");
    let loaded = trace_resource::Entity::find_by_id(resource.id)
        .one(&db)
        .await
        .expect("load resource")
        .expect("resource row");
    assert_eq!(
        (loaded.ordinal, loaded.byte_start, loaded.byte_end),
        (4, 32, 64)
    );
    let text = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT index_version, ascii_fold FROM trace_fragment WHERE id = $1",
            [fragment.id.into()],
        ))
        .await
        .expect("fragment sql")
        .expect("fragment row");
    assert_eq!(
        text.try_get::<i64>("", "index_version").expect("version"),
        1
    );
    assert_eq!(
        text.try_get::<String>("", "ascii_fold").expect("fold"),
        "hello"
    );
    let gin = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM pg_indexes \
             WHERE tablename = 'trace_fragment' AND indexdef ILIKE '%gin%'"
                .to_owned(),
        ))
        .await
        .expect("gin count")
        .expect("gin row");
    assert_eq!(gin.try_get::<i64>("", "n").expect("n"), 0);
    db.close().await.expect("close traces database");
}

const TRACE_SERVER_ID: &str = "00000000-0000-4000-8000-000000000050";

fn load_profile(path: &str) -> Config {
    let loaded = ConfigLoader::new(ConfigInput {
        cli_path: Some(PathBuf::from(path)),
        ..ConfigInput::default()
    })
    .load()
    .expect("resolve profile");
    let body = std::fs::read_to_string(&loaded.path).expect("read profile");
    // Sample storage-only mounts a deploy secret. Substitute only that
    // placeholder so the shipped `[traces]` table still loads through Config.
    let rendered = substitute_file_secrets(&body);
    Config::load_str(&rendered).unwrap_or_else(|err| panic!("load {path}: {err}"))
}

fn substitute_file_secrets(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    const OPEN: &str = "${file:";
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str("fixture-token");
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn sample_with_traces(replacement: &str) -> String {
    let body = std::fs::read_to_string("config/config.toml").expect("sample");
    let marker = "[traces]\nenabled = false\n";
    assert!(body.contains(marker), "sample traces block");
    body.replacen(marker, replacement, 1)
}

#[tokio::test]
async fn trace_config_model_profiles_stay_disabled() {
    for path in [
        "config/config.toml",
        "config/config-review.toml",
        "config/config-storage-only.toml",
        "config/config-storage-only.none.toml",
    ] {
        let config = load_profile(path);
        assert!(!config.traces.enabled, "{path}");
        assert!(config.traces.server_id.is_none(), "{path}");
        assert!(config.traces.scope.is_none(), "{path}");
    }
}

#[tokio::test]
async fn trace_config_model_explicit_enable_keeps_configured_server_id() {
    let rendered = sample_with_traces(&format!(
        "[traces]\nenabled = true\nserver_id = \"{TRACE_SERVER_ID}\"\nscope = \"fixture-a\"\n"
    ));
    let first = Config::load_str(&rendered).expect("first load");
    let second = Config::load_str(&rendered).expect("second load");
    first.validate().expect("explicit enable validates");
    second.validate().expect("second load validates");
    assert!(first.traces.enabled);
    assert_eq!(first.traces.server_id.as_deref(), Some(TRACE_SERVER_ID));
    assert_eq!(first.traces.scope.as_deref(), Some("fixture-a"));
    assert_eq!(first.traces.server_id, second.traces.server_id);
    assert_eq!(first.traces.scope, second.traces.scope);
}

#[tokio::test]
async fn trace_config_model_enabled_without_server_id_is_rejected() {
    let rendered = sample_with_traces("[traces]\nenabled = true\n");
    let config = Config::load_str(&rendered).expect("toml parses");
    let error = config.validate().expect_err("enabled requires server_id");
    let text = error.to_string();
    assert!(
        text.contains("traces.server_id"),
        "rejection names server_id: {text}"
    );
}

#[tokio::test]
async fn trace_config_model_rejects_budget_override() {
    let rendered = sample_with_traces("[traces]\nenabled = false\nmax_object_bytes = 1\n");
    let error = Config::load_str(&rendered).expect_err("budget key is unknown");
    let text = error.to_string();
    assert!(
        text.contains("max_object_bytes") || text.contains("unknown"),
        "unknown budget field: {text}"
    );
}

#[tokio::test]
async fn trace_config_model_reload_classifies_traces() {
    let current = load_profile("config/config.toml");
    let handle = ConfigHandle::new(current);
    let before = handle.snapshot().expect("snapshot");
    let mut candidate = before.as_ref().clone();
    candidate.traces.enabled = true;
    candidate.traces.server_id = Some(TRACE_SERVER_ID.to_owned());
    candidate.traces.scope = Some("fixture-a".to_owned());
    let report = handle.reload(candidate).expect("reload");
    assert!(report.restart_required_fields.contains(&"traces.enabled"));
    assert!(report.restart_required_fields.contains(&"traces.server_id"));
    assert!(report.restart_required_fields.contains(&"traces.scope"));
    let after = handle.snapshot().expect("snapshot after reload");
    assert!(!after.traces.enabled);
    assert!(after.traces.server_id.is_none());
    assert!(after.traces.scope.is_none());
}

#[tokio::test]
async fn import_target_remove_blackbox_deactivates_matching_target() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    use sea_orm::{ActiveModelTrait, DatabaseBackend, Statement};
    use url::Url;

    let (traces, db) = traces_db::TracesDb::migrated().await;
    let database_name: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT current_database() AS name".to_owned(),
        ))
        .await
        .expect("database name")
        .expect("row")
        .try_get("", "name")
        .expect("name");
    let admin = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
        "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
    });
    let mut database_url = Url::parse(&admin).expect("admin url");
    database_url.set_path(&database_name);
    let database_url = database_url.to_string();
    let temp = tempfile::tempdir().expect("temp");
    let config_path = temp.path().join("config.toml");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
        &config_path,
    )
    .expect("copy config");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("addr")
        .port();
    let stdout_path = temp.path().join("service.out");
    let stderr_path = temp.path().join("service.err");
    let mut service = Command::new(env!("CARGO_BIN_EXE_mega2"))
        .arg("--config")
        .arg(&config_path)
        .args(["service", "http", "--host", "127.0.0.1", "-p"])
        .arg(port.to_string())
        .env("MEGA_DATABASE__DB_URL", &database_url)
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_BASE_DIR", temp.path())
        .env("MEGA_CACHE_DIR", temp.path().join("cache"))
        .env("MEGA_LOG__PRINT_STD", "true")
        .stdout(Stdio::from(
            std::fs::File::create(&stdout_path).expect("stdout"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("stderr"),
        ))
        .spawn()
        .expect("spawn service");
    let deadline = Instant::now() + Duration::from_secs(90);
    let ready = loop {
        if Instant::now() >= deadline {
            break false;
        }
        if service.try_wait().expect("poll service").is_some() {
            break false;
        }
        let count: i64 = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT count(*)::bigint AS n FROM mega_refs".to_owned(),
            ))
            .await
            .expect("count refs")
            .expect("row")
            .try_get("", "n")
            .expect("n");
        if count > 0 {
            break true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let _ = service.kill();
    let _ = service.wait();
    assert!(
        ready,
        "service did not initialize\nstdout:\n{}\nstderr:\n{}",
        std::fs::read_to_string(&stdout_path).unwrap_or_default(),
        std::fs::read_to_string(&stderr_path).unwrap_or_default()
    );

    let path = "/third-party/blackbox";
    let repo_id = 910_048_i64;
    let target_id = Uuid::new_v4().to_string();
    mega2_core::git_repo::ActiveModel {
        id: Set(repo_id),
        repo_path: Set(path.to_owned()),
        repo_name: Set("blackbox".to_owned()),
        created_at: Set(Utc::now().naive_utc()),
        updated_at: Set(Utc::now().naive_utc()),
    }
    .insert(&db)
    .await
    .expect("seed repo");
    trace_target::ActiveModel {
        id: Set(target_id.clone()),
        target_kind: Set("import".to_owned()),
        project_path: Set(path.to_owned()),
        backing_import_repo_id: Set(Some(repo_id.to_string())),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("seed target");

    let remove = Command::new(env!("CARGO_BIN_EXE_mega2"))
        .arg("--config")
        .arg(&config_path)
        .args(["import-repo", "remove", "--path", path, "--yes"])
        .env("MEGA_DATABASE__DB_URL", &database_url)
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_BASE_DIR", temp.path())
        .env("MEGA_CACHE_DIR", temp.path().join("cache"))
        .env("MEGA_LOG__PRINT_STD", "true")
        .output()
        .expect("run remove");
    let stdout = String::from_utf8_lossy(&remove.stdout);
    let stderr = String::from_utf8_lossy(&remove.stderr);
    assert!(
        remove.status.success(),
        "remove failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&format!("removed {path}")),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let retired = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load")
        .expect("target");
    assert!(!retired.active);
    let generation: i64 = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(MAX((metadata->>'binding_generation')::bigint), 1) AS generation
             FROM audit_logs WHERE metadata->>'trace_target_id' = $1",
            [target_id.clone().into()],
        ))
        .await
        .expect("generation")
        .expect("row")
        .try_get("", "generation")
        .expect("generation");
    assert_eq!(generation, 2);
    let repo_left: i64 = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM git_repo WHERE id = $1",
            [repo_id.into()],
        ))
        .await
        .expect("repo count")
        .expect("row")
        .try_get("", "n")
        .expect("n");
    assert_eq!(repo_left, 0);

    let replacement = Uuid::new_v4().to_string();
    trace_target::ActiveModel {
        id: Set(replacement.clone()),
        target_kind: Set("import".to_owned()),
        project_path: Set(path.to_owned()),
        backing_import_repo_id: Set(Some("910049".to_owned())),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("replacement target");
    let again = Command::new(env!("CARGO_BIN_EXE_mega2"))
        .arg("--config")
        .arg(&config_path)
        .args(["import-repo", "remove", "--path", path, "--yes"])
        .env("MEGA_DATABASE__DB_URL", &database_url)
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_BASE_DIR", temp.path())
        .env("MEGA_CACHE_DIR", temp.path().join("cache"))
        .env("MEGA_LOG__PRINT_STD", "true")
        .output()
        .expect("run continuation");
    let again_out = String::from_utf8_lossy(&again.stdout);
    let again_err = String::from_utf8_lossy(&again.stderr);
    assert!(
        again.status.success(),
        "second remove failed\nstdout:\n{again_out}\nstderr:\n{again_err}"
    );
    let kept = trace_target::Entity::find_by_id(&replacement)
        .one(&db)
        .await
        .expect("load replacement")
        .expect("replacement");
    assert!(kept.active);
    assert_eq!(kept.id, replacement);
    let _ = traces;
}

fn rebind_output(
    cwd: &std::path::Path,
    args: &[&str],
    env: &[(&str, Option<&str>)],
) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mega2"));
    command
        .current_dir(cwd)
        .args(args)
        .env("MEGA_LOG__PRINT_STD", "true")
        .env_remove("MEGA_CONFIG")
        .env_remove("MEGA_PROFILE");
    for (key, value) in env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    command.output().expect("spawn mega2")
}

fn rebind_text(output: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn rebind_target_cli_help_lists_parent_and_subcommand() {
    let cwd = tempfile::tempdir().expect("cwd");
    let parent = rebind_output(cwd.path(), &["traces", "--help"], &[]);
    let (stdout, stderr) = rebind_text(&parent);
    assert!(
        parent.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("rebind-target"), "{stdout}");
    let child = rebind_output(cwd.path(), &["traces", "rebind-target", "--help"], &[]);
    let (stdout, stderr) = rebind_text(&child);
    assert!(
        child.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("--target"), "{stdout}");
    assert!(stdout.contains("--path"), "{stdout}");
}

#[test]
fn rebind_target_cli_refuses_unnamed_or_missing_config() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("cwd");
    let base = root.path().join("base");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&base).expect("base");
    let missing = rebind_output(
        &cwd,
        &[
            "traces",
            "rebind-target",
            "--target",
            "00000000-0000-0000-0000-000000000001",
            "--path",
            "/project",
        ],
        &[("MEGA_BASE_DIR", Some(base.to_str().expect("utf-8")))],
    );
    let (stdout, stderr) = rebind_text(&missing);
    assert!(
        !missing.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("will not generate a default"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!base.join("etc/config.toml").exists());
    assert!(!cwd.join("config/config.toml").exists());

    let discovered = cwd.join("config");
    std::fs::create_dir_all(&discovered).expect("config dir");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
        discovered.join("config.toml"),
    )
    .expect("copy cwd config");
    let cwd_config = rebind_output(
        &cwd,
        &[
            "traces",
            "rebind-target",
            "--target",
            "00000000-0000-0000-0000-000000000001",
            "--path",
            "/project",
        ],
        &[("MEGA_BASE_DIR", Some(base.to_str().expect("utf-8")))],
    );
    let (stdout, stderr) = rebind_text(&cwd_config);
    assert!(
        !cwd_config.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("was not named") && stderr.contains("source: cwd"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[tokio::test]
async fn rebind_target_cli_rebinds_existing_target_and_rejects_bad_paths() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };

    use sea_orm::{ActiveModelTrait, DatabaseBackend, Statement};
    use url::Url;

    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let database_name: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT current_database() AS name".to_owned(),
        ))
        .await
        .expect("database name")
        .expect("row")
        .try_get("", "name")
        .expect("name");
    let admin = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
        "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
    });
    let mut database_url = Url::parse(&admin).expect("admin url");
    database_url.set_path(&database_name);
    let database_url = database_url.to_string();
    let temp = tempfile::tempdir().expect("temp");
    let config_path = temp.path().join("config.toml");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
        &config_path,
    )
    .expect("copy config");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("addr")
        .port();
    let stdout_path = temp.path().join("service.out");
    let stderr_path = temp.path().join("service.err");
    let mut service = std::process::Command::new(env!("CARGO_BIN_EXE_mega2"))
        .arg("--config")
        .arg(&config_path)
        .args(["service", "http", "--host", "127.0.0.1", "-p"])
        .arg(port.to_string())
        .env("MEGA_DATABASE__DB_URL", &database_url)
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_BASE_DIR", temp.path())
        .env("MEGA_CACHE_DIR", temp.path().join("cache"))
        .env("MEGA_LOG__PRINT_STD", "true")
        .env_remove("MEGA_CONFIG")
        .stdout(Stdio::from(
            std::fs::File::create(&stdout_path).expect("stdout"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("stderr"),
        ))
        .spawn()
        .expect("spawn service");
    let deadline = Instant::now() + Duration::from_secs(90);
    let ready = loop {
        if Instant::now() >= deadline || service.try_wait().expect("poll service").is_some() {
            break false;
        }
        let count: i64 = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT count(*)::bigint AS n FROM mega_refs".to_owned(),
            ))
            .await
            .expect("count refs")
            .expect("row")
            .try_get("", "n")
            .expect("n");
        if count > 0 {
            break true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let _ = service.kill();
    let _ = service.wait();
    assert!(
        ready,
        "service did not initialize\nstdout:\n{}\nstderr:\n{}",
        std::fs::read_to_string(&stdout_path).unwrap_or_default(),
        std::fs::read_to_string(&stderr_path).unwrap_or_default()
    );

    let target_id = Uuid::new_v4().to_string();
    trace_target::ActiveModel {
        id: Set(target_id.clone()),
        target_kind: Set("native".to_owned()),
        project_path: Set("/project".to_owned()),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("seed target");

    let config = config_path.to_str().expect("utf-8 config");
    let base = temp.path().to_str().expect("utf-8 base");
    let run = |args: &[&str], mega_config: bool| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mega2"));
        command
            .current_dir(temp.path())
            .env("MEGA_DATABASE__DB_URL", &database_url)
            .env("MEGA_DATABASE__DB_TYPE", "postgres")
            .env("MEGA_BASE_DIR", base)
            .env("MEGA_CACHE_DIR", temp.path().join("cache"))
            .env("MEGA_LOG__PRINT_STD", "true")
            .env_remove("MEGA_PROFILE");
        if mega_config {
            command.env("MEGA_CONFIG", config);
        } else {
            command.env_remove("MEGA_CONFIG");
            command.arg("--config").arg(config);
        }
        command.args(args).output().expect("run rebind")
    };

    let moved = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            &target_id,
            "--path",
            "/third-party",
        ],
        false,
    );
    let (stdout, stderr) = rebind_text(&moved);
    assert!(
        moved.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line == format!("rebound {target_id} /third-party")),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let row = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load")
        .expect("target");
    assert_eq!(row.project_path, "/third-party");
    assert_eq!(row.id, target_id);
    assert!(row.active);

    let slash = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            &target_id,
            "--path",
            "/third-party/",
        ],
        false,
    );
    let (stdout, stderr) = rebind_text(&slash);
    assert!(
        !slash.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not a canonical project path"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let row = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load after slash")
        .expect("target");
    assert_eq!(row.project_path, "/third-party");

    let missing_path = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            &target_id,
            "--path",
            "/project/missing",
        ],
        false,
    );
    let (stdout, stderr) = rebind_text(&missing_path);
    assert!(
        !missing_path.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not an existing directory"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let row = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load after missing path")
        .expect("target");
    assert_eq!(row.project_path, "/third-party");

    let unknown = "00000000-0000-0000-0000-000000000099";
    let missing_target = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            unknown,
            "--path",
            "/project",
        ],
        false,
    );
    let (stdout, stderr) = rebind_text(&missing_target);
    assert!(
        !missing_target.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("target does not exist"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        trace_target::Entity::find_by_id(unknown)
            .one(&db)
            .await
            .expect("lookup unknown")
            .is_none()
    );

    let via_env = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            &target_id,
            "--path",
            "/project",
        ],
        true,
    );
    let (stdout, stderr) = rebind_text(&via_env);
    assert!(
        via_env.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let row = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load after env rebind")
        .expect("target");
    assert_eq!(row.id, target_id);
    assert_eq!(row.project_path, "/project");
    assert!(row.active);
}

#[allow(
    dead_code,
    reason = "shared git-cli helper also contains SSH/LFS/auth probes unused by traces HTTP cases"
)]
#[path = "common/git_cli.rs"]
mod git_cli;

mod disabled_http {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        panic::AssertUnwindSafe,
        path::Path,
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };

    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use futures::FutureExt;
    use sea_orm::{AccessMode, IsolationLevel, TransactionTrait};
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const LIMIT: Duration = Duration::from_secs(90);
    const SENTINEL: &[u8] = b"mtr94 ordinary Git sentinel\n";
    const SYNTHETIC_BASIC: &str = "Basic bXRyOTQ6c3ludGhldGljLWludmFsaWQtdG9rZW4=";
    const SYNTHETIC_BEARER: &str = "Bearer mtr94-synthetic-invalid-token";
    const LFS_CONTENT_TYPE: &str = "application/vnd.git-lfs+json";
    type Snapshot = BTreeMap<String, Vec<String>>;

    #[derive(Clone, Copy)]
    pub enum Endpoint {
        Discovery,
        ReceiveDiscovery,
        Upload,
        Receive,
    }

    impl Endpoint {
        fn method(self) -> &'static str {
            match self {
                Self::Discovery | Self::ReceiveDiscovery => "GET",
                Self::Upload | Self::Receive => "POST",
            }
        }

        fn content_type(self) -> Option<&'static str> {
            match self {
                Self::Discovery | Self::ReceiveDiscovery => None,
                Self::Upload => Some("application/x-git-upload-pack-request"),
                Self::Receive => Some("application/x-git-receive-pack-request"),
            }
        }

        fn suffix(self) -> &'static str {
            match self {
                Self::Discovery => "/info/refs?service=git-upload-pack",
                Self::ReceiveDiscovery => "/info/refs?service=git-receive-pack",
                Self::Upload => "/git-upload-pack",
                Self::Receive => "/git-receive-pack",
            }
        }
    }

    struct OwnedChild(Option<Child>);

    impl OwnedChild {
        fn spawn(command: &mut Command) -> Self {
            Self(Some(
                command.spawn().expect("spawn isolated fixture process"),
            ))
        }

        fn stop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                child.wait().expect("reap isolated fixture process");
            }
        }

        async fn require_success(&mut self) {
            let deadline = Instant::now() + LIMIT;
            loop {
                let child = self.0.as_mut().expect("owned child");
                if let Some(status) = child.try_wait().expect("poll fixture process") {
                    self.0.take();
                    assert!(status.success(), "fixture command failed: {status}");
                    return;
                }
                assert!(Instant::now() < deadline, "fixture command exceeded 90s");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            self.stop();
        }
    }

    struct Fixture {
        temp: tempfile::TempDir,
        shared: tempfile::TempDir,
        database_url: String,
        db: sea_orm::DatabaseConnection,
        service: OwnedChild,
        port: u16,
        scope: String,
        epoch: String,
        epoch_sha256: String,
        scope_sha256: String,
        source: String,
        tip: String,
        pack: Vec<u8>,
        traces_auth: bool,
        static_auth: bool,
    }

    impl Fixture {
        async fn new(db: &sea_orm::DatabaseConnection) -> Self {
            assert!(!git_cli::git_cli_skip_requested(), "Docker Git is required");
            assert_ne!(
                std::env::var("MEGA2_IT_ALLOW_HOST_GIT").as_deref(),
                Ok("1"),
                "host Git fallback is not this oracle"
            );
            git_cli::require_git_cli_runner();
            let work = git_cli::git_cli_workdir();
            fs::create_dir_all(&work).expect("shared Git work root");
            let shared = tempfile::Builder::new()
                .prefix("traces-http-")
                .tempdir_in(work)
                .expect("isolated Docker Git workdir");
            let temp = tempfile::tempdir().expect("HTTP fixture directory");
            let mut config: toml::Value = toml::from_str(
                &fs::read_to_string(
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
                )
                .expect("read fixture config"),
            )
            .expect("parse fixture config");
            config
                .get_mut("git")
                .and_then(toml::Value::as_table_mut)
                .expect("Git config table")
                .remove("push_auth");
            fs::write(
                temp.path().join("config.toml"),
                toml::to_string(&config).expect("serialize fixture config"),
            )
            .expect("write fixture config");
            let name = scalar(db, "SELECT current_database() AS value").await;
            let admin = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
                "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
            });
            let mut url = url::Url::parse(&admin).expect("test database URL");
            url.set_path(&name);
            Self {
                temp,
                shared,
                database_url: url.to_string(),
                db: db.clone(),
                service: OwnedChild(None),
                port: 0,
                scope: Uuid::new_v4().to_string(),
                epoch: Uuid::new_v4().to_string(),
                epoch_sha256: Uuid::new_v4().to_string(),
                scope_sha256: Uuid::new_v4().to_string(),
                source: "mtr94-source".to_owned(),
                tip: String::new(),
                pack: Vec::new(),
                traces_auth: false,
                static_auth: false,
            }
        }

        fn command(&self, enabled: bool, policy: &str, import_dir: &str) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_mega2"));
            command.env_clear().current_dir(self.temp.path());
            for key in ["PATH", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
            command
                .arg("--config")
                .arg(self.temp.path().join("config.toml"))
                .env("MEGA_BASE_DIR", self.temp.path().join("base"))
                .env("MEGA_CACHE_DIR", self.temp.path().join("cache"))
                .env("MEGA_DATABASE__DB_TYPE", "postgres")
                .env("MEGA_DATABASE__DB_URL", &self.database_url)
                .env("MEGA_DATABASE__MAX_CONNECTION", "4")
                .env("MEGA_DATABASE__MIN_CONNECTION", "1")
                .env("MEGA_DATABASE__ACQUIRE_TIMEOUT", "5")
                .env("MEGA_DATABASE__CONNECT_TIMEOUT", "5")
                .env("MEGA_DATABASE__SQLX_LOGGING", "false")
                .env(
                    "MEGA_REDIS__URL",
                    std::env::var("MEGA_REDIS__URL")
                        .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_owned()),
                )
                .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
                .env(
                    "MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR",
                    self.temp.path().join("objects"),
                )
                .env("MEGA_MONOREPO__PUSH_POLICY", policy)
                .env("MEGA_MONOREPO__OBJECT_FORMAT", "sha1")
                .env("MEGA_MONOREPO__IMPORT_DIR", import_dir)
                .env(
                    "MEGA_MONOREPO__ROOT_DIRS",
                    "third-party,project,%2etraces,.traces-backup,.tracesx,.TRACES,.traces.git",
                )
                .env("MEGA_GIT__ANONYMOUS_ACCESS", "true")
                .env(
                    "MEGA_CEDAR__ENFORCEMENT",
                    if self.traces_auth && !self.static_auth {
                        "enforce"
                    } else {
                        "off"
                    },
                )
                .env("MEGA_TRACES__ENABLED", enabled.to_string())
                .env("MEGA_TRACES__SERVER_ID", TRACE_SERVER_ID)
                .env("MEGA_NOTIFICATION__ENABLED", "false")
                .env("MEGA_BUCK__ENABLE_SESSION_CLEANUP", "false")
                .env("MEGA_ARTIFACTS_GC__ENABLE", "false")
                .env("MEGA_VIEWS__ENABLED", "false")
                .env("MEGA_MST2__ENABLED", "false")
                .env("MEGA_STORAGE_EVENTS__ENABLED", "false")
                .env("MEGA_LOG__PRINT_STD", "true")
                .env("MEGA_LOG__WITH_ANSI", "false")
                .stdin(Stdio::null())
                .stdout(Stdio::from(
                    fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.temp.path().join("process.out"))
                        .expect("stdout file"),
                ))
                .stderr(Stdio::from(
                    fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.temp.path().join("process.err"))
                        .expect("stderr file"),
                ));
            if policy == "trunk" {
                command.env(
                    "MEGA_GIT__PUSH_AUTH",
                    if self.static_auth { "token" } else { "none" },
                );
            }
            if self.traces_auth {
                let spool = self.temp.path().join("owned-spool");
                fs::create_dir_all(&spool).expect("owned HTTP spool");
                command.env("TMPDIR", spool);
            }
            command
        }

        async fn start(&mut self, enabled: bool, policy: &str, import_dir: &str) {
            self.stop();
            self.port = git_cli::reserve_ephemeral_port();
            let mut command = self.command(enabled, policy, import_dir);
            command.args([
                "service",
                "http",
                "--host",
                git_cli::service_listen_host(),
                "-p",
            ]);
            command.arg(self.port.to_string());
            self.service = OwnedChild::spawn(&mut command);
            git_cli::record_service_pid(self.service.0.as_ref().expect("child").id());
            let deadline = Instant::now() + LIMIT;
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("readiness client");
            loop {
                assert!(
                    self.service
                        .0
                        .as_mut()
                        .expect("child")
                        .try_wait()
                        .expect("poll service")
                        .is_none(),
                    "HTTP service exited before readiness"
                );
                if let Ok(response) = client
                    .get(git_cli::mega2_host_http_url(self.port, "/api/openapi.json"))
                    .send()
                    .await
                    && response.status() == reqwest::StatusCode::OK
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "HTTP service readiness exceeded 90s"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
                while !self.initial_index_ready().await {
                    assert!(
                        self.service
                            .0
                            .as_mut()
                            .expect("child")
                            .try_wait()
                            .expect("poll service")
                            .is_none(),
                        "HTTP service exited before initial index readiness"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("HTTP listener and initial index readiness exceeded 90s");
        }

        async fn initial_index_ready(&self) -> bool {
            let txn = self
                .db
                .begin_with_config(
                    Some(IsolationLevel::RepeatableRead),
                    Some(AccessMode::ReadOnly),
                )
                .await
                .expect("initial index snapshot");
            let root: String = txn
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT ref_tree_hash FROM mega_refs WHERE path='/' AND ref_name='refs/heads/main'",
                ))
                .await
                .expect("fixture root query")
                .expect("initialized fixture root")
                .try_get("", "ref_tree_hash")
                .expect("fixture root tree");
            let oid_bytes = match root.len() {
                40 => 20,
                64 => 32,
                _ => panic!("invalid fixture root tree identity"),
            };
            let mut stack = vec![(root, String::new())];
            let mut expected = BTreeSet::new();
            let mut final_paths = BTreeMap::new();
            while let Some((oid, parent)) = stack.pop() {
                let body: Vec<u8> = txn
                    .query_one_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "SELECT sub_trees FROM mega_tree WHERE tree_id=$1",
                        [oid.into()],
                    ))
                    .await
                    .expect("fixture raw tree query")
                    .expect("fixture tree exists")
                    .try_get("", "sub_trees")
                    .expect("fixture raw tree bytes");
                let mut remaining = body.as_slice();
                while !remaining.is_empty() {
                    let nul = remaining
                        .iter()
                        .position(|byte| *byte == 0)
                        .expect("fixture tree entry terminator");
                    let (mode, name) = std::str::from_utf8(&remaining[..nul])
                        .expect("fixture tree entry text")
                        .split_once(' ')
                        .expect("fixture tree mode/name");
                    let end = nul + 1 + oid_bytes;
                    assert!(end <= remaining.len(), "fixture tree object identity");
                    let child = hex::encode(&remaining[nul + 1..end]);
                    let path = format!("{parent}/{name}");
                    if mode == "40000" || mode == "040000" {
                        stack.push((child, path));
                    } else {
                        expected.insert((child.clone(), path.clone()));
                        final_paths.insert(child, path);
                    }
                    remaining = &remaining[end..];
                }
            }
            assert!(!expected.is_empty(), "initialized fixture root has blobs");
            let actual: BTreeSet<(String, String)> = txn
                .query_all_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT blob_id, path FROM blob_paths",
                ))
                .await
                .expect("initial blob paths query")
                .into_iter()
                .map(|row| {
                    (
                        row.try_get("", "blob_id").expect("initial blob identity"),
                        row.try_get("", "path").expect("initial blob path"),
                    )
                })
                .collect();
            let legacy: BTreeMap<String, String> = txn
                .query_all_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT blob_id, file_path FROM mega_blob",
                ))
                .await
                .expect("initial legacy paths query")
                .into_iter()
                .map(|row| {
                    (
                        row.try_get("", "blob_id").expect("legacy blob identity"),
                        row.try_get("", "file_path").expect("legacy blob path"),
                    )
                })
                .collect();
            let pending: i64 = txn
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT count(*) AS pending FROM push_queue WHERE status::text IN ('Queued','Running')",
                ))
                .await
                .expect("initial queue query")
                .expect("initial queue row")
                .try_get("", "pending")
                .expect("initial queue count");
            txn.commit().await.expect("finish initial index snapshot");
            pending == 0
                && actual == expected
                && final_paths
                    .iter()
                    .all(|(oid, path)| legacy.get(oid) == Some(path))
        }

        fn stop(&mut self) {
            self.service.stop();
            if self.port != 0 {
                git_cli::wait_until_port_closed(self.port, Duration::from_secs(5));
            }
        }

        fn git(&self, args: &[&str]) -> Vec<u8> {
            let output = git_cli::git_cli_no_auth(self.shared.path(), args);
            assert!(
                output.status.success(),
                "Docker Git fixture command failed ({}) args={args:?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        fn git_text(&self, args: &[&str]) -> String {
            String::from_utf8(self.git(args))
                .expect("Git text output")
                .trim()
                .to_owned()
        }

        async fn prepare(&mut self, db: &sea_orm::DatabaseConnection) {
            let mut init = self.command(false, "trunk", "/third-party");
            init.args(["service", "init", "--yes"]);
            OwnedChild::spawn(&mut init).require_success().await;
            self.start(true, "trunk", "/third-party").await;
            let root_url = git_cli::mega2_http_url(self.port, "/");
            let native_alias = format!("%2etraces/by-scope/{}", self.scope);
            let scope_dir = format!("by-scope/{}", self.scope);
            let roots: [(&str, Vec<&str>); 6] = [
                ("project", vec!["control", ".traces/repo"]),
                (".traces-backup", vec!["repo"]),
                (".tracesx", vec!["repo"]),
                (".TRACES", vec!["repo"]),
                (".traces.git", vec!["ordinary"]),
                ("%2etraces", vec![&scope_dir]),
            ];
            for (index, (root, directories)) in roots.into_iter().enumerate() {
                let source = format!("native-source-{index}");
                let new_child = root == ".traces.git";
                let url = git_cli::mega2_http_url(
                    self.port,
                    &if new_child {
                        "/.traces.git/ordinary.git".to_owned()
                    } else {
                        format!("/{root}.git")
                    },
                );
                let source_url = if new_child {
                    git_cli::mega2_http_url(self.port, "/project.git")
                } else {
                    url.clone()
                };
                self.git(&["clone", &source_url, &source]);
                for directory in &directories {
                    let path = self.shared.path().join(&source).join(if new_child {
                        ""
                    } else {
                        directory
                    });
                    fs::create_dir_all(&path).expect("native subtree");
                    fs::write(path.join("sentinel.txt"), SENTINEL).expect("native sentinel");
                }
                self.git(&["-C", &source, "add", "."]);
                self.git(&[
                    "-C",
                    &source,
                    "-c",
                    "user.name=Traces Test",
                    "-c",
                    "user.email=traces@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "MTR94 native sentinel",
                ]);
                self.git(&[
                    "-C",
                    &source,
                    "push",
                    "--no-thin",
                    &url,
                    "HEAD:refs/heads/main",
                ]);
                for (child_index, directory) in directories.into_iter().enumerate() {
                    let child_url =
                        git_cli::mega2_http_url(self.port, &format!("/{root}/{directory}.git"));
                    let check = format!("native-target-{index}-{child_index}");
                    self.git(&["clone", &child_url, &check]);
                    assert_eq!(
                        fs::read(self.shared.path().join(check).join("sentinel.txt"))
                            .expect("native target clone bytes"),
                        SENTINEL
                    );
                }
            }
            let root_tip = scalar(db, "SELECT ref_commit_hash AS value FROM mega_refs WHERE path='/' AND ref_name='refs/heads/main'").await;
            let mut fetch = Vec::new();
            pkt(&mut fetch, "command=fetch\n");
            fetch.extend_from_slice(b"0001");
            pkt(&mut fetch, &format!("want {root_tip}\n"));
            pkt(&mut fetch, "done\n");
            fetch.extend_from_slice(b"0000");
            let root_fetch =
                request(self.port, Endpoint::Upload, "", true, &fetch, None, false).await;
            assert_eq!(
                root_fetch.status,
                200,
                "ordinary root fetch failed: {}",
                String::from_utf8_lossy(&root_fetch.body)
            );
            self.git(&["clone", &root_url, "native-check"]);
            assert_eq!(
                fs::read(
                    self.shared
                        .path()
                        .join("native-check/project/control/sentinel.txt")
                )
                .expect("native clone bytes"),
                SENTINEL
            );
            assert_eq!(
                fs::read(
                    self.shared
                        .path()
                        .join("native-check")
                        .join(&native_alias)
                        .join("sentinel.txt")
                )
                .expect("native alias root tree bytes"),
                SENTINEL
            );
            self.git(&[
                "init",
                "--initial-branch=main",
                "--object-format=sha1",
                "import",
            ]);
            fs::write(self.shared.path().join("import/sentinel.txt"), SENTINEL)
                .expect("import sentinel");
            self.git(&["-C", "import", "add", "sentinel.txt"]);
            self.git(&[
                "-C",
                "import",
                "-c",
                "user.name=Traces Test",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "MTR94 import sentinel",
            ]);
            let import_url = git_cli::mega2_http_url(self.port, "/third-party/mtr94-control.git");
            self.git(&["-C", "import", "push", &import_url, "HEAD:refs/heads/main"]);
            self.git(&["clone", &import_url, "import-check"]);
            assert_eq!(
                fs::read(self.shared.path().join("import-check/sentinel.txt"))
                    .expect("import clone bytes"),
                SENTINEL
            );
            self.tip = self.git_text(&["-C", "import", "rev-parse", "HEAD"]);
            self.pack = self.git(&[
                "-C",
                "import",
                "pack-objects",
                "--stdout",
                "--revs",
                "--all",
                "--window=0",
                "--no-reuse-delta",
                "--no-reuse-object",
            ]);
            assert!(self.pack.starts_with(b"PACK"));
            for path in [
                "/project/control.git",
                "/project/.traces/repo.git",
                "/.traces-backup/repo.git",
                "/.tracesx/repo.git",
                "/.TRACES/repo.git",
                "/.traces.git/ordinary.git",
            ] {
                for version in ["0", "2"] {
                    let refs = self.git_text(&[
                        "-c",
                        &format!("protocol.version={version}"),
                        "ls-remote",
                        &git_cli::mega2_http_url(self.port, path),
                    ]);
                    assert!(
                        refs.contains("refs/heads/main"),
                        "ordinary path missing main"
                    );
                }
            }
            let principal = Uuid::new_v4().to_string();
            let target = Uuid::new_v4().to_string();
            trace_principal::ActiveModel {
                id: Set(principal.clone()),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .expect("seed trace principal");
            trace_target::ActiveModel {
                id: Set(target.clone()),
                target_kind: Set("native".to_owned()),
                project_path: Set("/project/control".to_owned()),
                backing_import_repo_id: Set(None),
                active: Set(true),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .expect("seed trace target");
            trace_stream::ActiveModel {
                id: Set(self.scope.clone()),
                principal_id: Set(principal.clone()),
                target_id: Set(target.clone()),
                source_repo_id: Set(self.source.clone()),
                epoch_id: Set(self.epoch.clone()),
                hash_kind: Set("sha1".to_owned()),
                tip_oid: Set(None),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .expect("seed trace stream");
            trace_stream::ActiveModel {
                id: Set(self.scope_sha256.clone()),
                principal_id: Set(principal),
                target_id: Set(target),
                source_repo_id: Set(self.source.clone()),
                epoch_id: Set(self.epoch_sha256.clone()),
                hash_kind: Set("sha256".to_owned()),
                tip_oid: Set(None),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .expect("seed sha256 trace stream");
            self.start(false, "trunk", "/third-party").await;
            self.converge(db).await;
            for (index, path) in [
                "/project/control.git",
                "/project/.traces/repo.git",
                "/.traces-backup/repo.git",
                "/.tracesx/repo.git",
                "/.TRACES/repo.git",
                "/.traces.git/ordinary.git",
                "/third-party/mtr94-control.git",
            ]
            .into_iter()
            .enumerate()
            {
                let url = git_cli::mega2_http_url(self.port, path);
                for version in ["0", "2"] {
                    let refs = self.git_text(&[
                        "-c",
                        &format!("protocol.version={version}"),
                        "ls-remote",
                        &url,
                    ]);
                    assert!(
                        refs.contains("refs/heads/main"),
                        "disabled traces guard rejected ordinary path {path}"
                    );
                }
                let check = format!("disabled-ordinary-{index}");
                self.git(&["clone", &url, &check]);
                assert_eq!(
                    fs::read(self.shared.path().join(check).join("sentinel.txt"))
                        .expect("ordinary clone bytes with traces disabled"),
                    SENTINEL
                );
            }
        }

        async fn converge(&self, db: &sea_orm::DatabaseConnection) {
            let deadline = Instant::now() + LIMIT;
            loop {
                let pending = scalar(db, "SELECT count(*)::text AS value FROM push_queue WHERE status::text IN ('Queued','Running')").await;
                if pending == "0" {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture queue did not finish within 90s"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let root_url = git_cli::mega2_http_url(self.port, "/");
            self.git(&["-C", "native-check", "fetch", &root_url, "main"]);
            let root_tip = self.git_text(&["-C", "native-check", "rev-parse", "FETCH_HEAD"]);
            assert_eq!(scalar(db, "SELECT ref_commit_hash AS value FROM mega_refs WHERE path='/' AND ref_name='refs/heads/main'").await, root_tip);
            let tree = self.git_text(&["-C", "native-check", "ls-tree", "-r", "-t", "FETCH_HEAD"]);
            let mut entries: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
            for line in tree.lines() {
                let (header, path) = line.split_once('\t').expect("ls-tree record");
                let parts: Vec<_> = header.split_whitespace().collect();
                let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                entries.entry(parent.to_owned()).or_default().push((
                    parts[1].to_owned(),
                    parts[2].to_owned(),
                    path.to_owned(),
                ));
            }
            let mut stack = vec![String::new()];
            let mut expected = BTreeSet::new();
            let mut final_paths = BTreeMap::new();
            while let Some(parent) = stack.pop() {
                for (kind, oid, path) in entries.get(&parent).into_iter().flatten() {
                    if kind == "tree" {
                        stack.push(path.clone());
                    } else {
                        let absolute = format!("/{path}");
                        expected.insert((oid.clone(), absolute.clone()));
                        final_paths.insert(oid.clone(), absolute);
                    }
                }
            }
            assert!(!expected.is_empty(), "real root tree must have blobs");
            loop {
                let pending = scalar(db, "SELECT count(*)::text AS value FROM push_queue WHERE status::text IN ('Queued','Running')").await;
                let rows = db
                    .query_all_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        "SELECT blob_id, path FROM blob_paths".to_owned(),
                    ))
                    .await
                    .expect("blob paths convergence query");
                let actual: BTreeSet<(String, String)> = rows
                    .iter()
                    .map(|row| {
                        (
                            row.try_get("", "blob_id").expect("blob id"),
                            row.try_get("", "path").expect("blob path"),
                        )
                    })
                    .collect();
                let blobs = db
                    .query_all_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        "SELECT blob_id, file_path FROM mega_blob".to_owned(),
                    ))
                    .await
                    .expect("blob filepath convergence query");
                let paths_match = blobs.iter().all(|row| {
                    let oid: String = row.try_get("", "blob_id").expect("blob oid");
                    let path: String = row.try_get("", "file_path").expect("blob filepath");
                    final_paths
                        .get(&oid)
                        .is_none_or(|expected| expected == &path)
                });
                if pending == "0" && actual == expected && paths_match {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture queue/root/blob_paths did not converge within 90s"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        fn locators(&self) -> Vec<String> {
            let target = URL_SAFE_NO_PAD.encode("/project/control");
            let source = URL_SAFE_NO_PAD.encode(&self.source);
            let scope = &self.scope;
            let canonical = format!("/.traces/by-scope/{scope}.git");
            vec![
                canonical.clone(),
                format!("/.traces/by-scope/{}.git", self.scope_sha256),
                format!("/.traces/by-scope/{}.git", Uuid::new_v4()),
                format!(
                    "/.traces/by-path/{target}/self/{source}/{}/sha1.git",
                    self.epoch
                ),
                format!(
                    "/.traces/by-path/{target}/self/{source}/{}/sha256.git",
                    self.epoch_sha256
                ),
                format!(
                    "/.traces/by-path/{target}/self/{source}/{}/sha1.git",
                    Uuid::new_v4()
                ),
                format!(
                    "/.traces/by-path/{target}/self/{source}/{}/sha256.git",
                    Uuid::new_v4()
                ),
                format!("/{canonical}"),
                format!("/./{}", &canonical[1..]),
                format!("/other/..{canonical}"),
                "/.traces/../project/control.git".to_owned(),
                "//.traces/../project/control.git".to_owned(),
                format!("/%2etraces/by-scope/{scope}.git"),
                format!("/.%74races/by-scope/{scope}.git"),
                format!("/%2Etraces/by-scope/{scope}.git"),
                format!("/.traces%2Fby-scope/{scope}.git"),
                format!("/%2f.traces/by-scope/{scope}.git"),
                format!("/other/%2e%2e{canonical}"),
                "/.traces/by-scope".to_owned(),
                "/.traces/by-scope/not-a-uuid.git".to_owned(),
                format!("/.traces/by-scope/{}.git", scope.to_uppercase()),
                format!("/.traces/by-scope/{scope}/extra.git"),
                format!(
                    "/.traces/by-path/{target}=/self/{source}/{}/sha1.git",
                    self.epoch
                ),
                format!(
                    "/.traces/by-path/{target}/self/{source}/{}/sha512.git",
                    self.epoch
                ),
                format!("/.traces/by-scope/{scope}"),
                "/.traces/%FF/bad.git".to_owned(),
                "/.traces/%GG/bad.git".to_owned(),
                "/.traces.git".to_owned(),
            ]
        }

        fn request_body(&self, endpoint: Endpoint, v2: bool) -> Vec<u8> {
            match endpoint {
                Endpoint::Discovery | Endpoint::ReceiveDiscovery => Vec::new(),
                Endpoint::Upload => {
                    let mut body = Vec::new();
                    if v2 {
                        pkt(&mut body, "command=fetch\n");
                        body.extend_from_slice(b"0001");
                        pkt(&mut body, &format!("want {}\n", self.tip));
                        pkt(&mut body, "done\n");
                        body.extend_from_slice(b"0000");
                    } else {
                        pkt(&mut body, &format!("want {}\n", self.tip));
                        body.extend_from_slice(b"0000");
                        pkt(&mut body, "done\n");
                    }
                    body
                }
                Endpoint::Receive => {
                    let mut body = Vec::new();
                    pkt(
                        &mut body,
                        &format!(
                            "{} {} refs/libra/traces\0report-status\n",
                            "0".repeat(40),
                            self.tip
                        ),
                    );
                    body.extend_from_slice(b"0000");
                    body.extend_from_slice(&self.pack);
                    body
                }
            }
        }

        async fn disabled(&self, db: &sea_orm::DatabaseConnection, endpoint: Endpoint) {
            let before = snapshot(db).await;
            let mut common = None;
            for locator in self.locators() {
                for v2 in [false, true] {
                    let response = request(
                        self.port,
                        endpoint,
                        &locator,
                        v2,
                        &self.request_body(endpoint, v2),
                        None,
                        false,
                    )
                    .await;
                    response.refused(&self.tip);
                    if let Some(expected) = &common {
                        assert_eq!(
                            &response.body, expected,
                            "disabled responses disclose locator differences"
                        );
                    } else {
                        common = Some(response.body.clone());
                    }
                    unchanged(&before, &snapshot(db).await);
                }
            }
            for prefix in [".traces", "%2etraces"] {
                let path = format!("/{prefix}/by-scope/{}.git{}", self.scope, endpoint.suffix());
                for authorization in [SYNTHETIC_BASIC, SYNTHETIC_BEARER] {
                    for v2 in [false, true] {
                        let mut headers = vec![("Authorization", authorization)];
                        if v2 {
                            headers.push(("Git-Protocol", "version=2"));
                        }
                        let response = raw_request(
                            self.port,
                            endpoint.method(),
                            &path,
                            endpoint.content_type(),
                            &self.request_body(endpoint, v2),
                            &headers,
                            false,
                        )
                        .await;
                        response.refused(&self.tip);
                        assert_eq!(
                            Some(&response.body),
                            common.as_ref(),
                            "credentials must not affect disabled refusal"
                        );
                        unchanged(&before, &snapshot(db).await);
                    }
                }
            }
            for encoding in ["unknown-mtr94-encoding", "gzip"] {
                let response = request(
                    self.port,
                    endpoint,
                    &format!("/.traces/by-scope/{}.git", self.scope),
                    false,
                    b"invalid-compressed-body",
                    Some(encoding),
                    false,
                )
                .await;
                response.refused(&self.tip);
                unchanged(&before, &snapshot(db).await);
            }
            let response = request(
                self.port,
                endpoint,
                &format!("/.traces/by-scope/{}.git", self.scope),
                false,
                b"",
                None,
                true,
            )
            .await;
            response.refused(&self.tip);
            unchanged(&before, &snapshot(db).await);
            if matches!(endpoint, Endpoint::Receive) {
                let response = request(
                    self.port,
                    endpoint,
                    &format!("/.traces/by-scope/{}.git", self.scope),
                    false,
                    b"0000",
                    None,
                    false,
                )
                .await;
                response.refused(&self.tip);
                unchanged(&before, &snapshot(db).await);
            }
        }

        async fn lfs_controls(&self, db: &sea_orm::DatabaseConnection, enabled: bool) {
            let before = snapshot(db).await;
            let mut paths = vec!["/project/control.git/info/lfs/objects/batch".to_owned()];
            if enabled {
                paths.push(format!(
                    "/%2etraces/by-scope/{}.git/info/lfs/objects/batch",
                    self.scope
                ));
            }
            for path in paths {
                for operation in ["download", "upload"] {
                    let body = lfs_batch(operation);
                    let response = raw_request(
                        self.port,
                        "POST",
                        &path,
                        Some(LFS_CONTENT_TYPE),
                        &body,
                        &[],
                        false,
                    )
                    .await;
                    assert_eq!(
                        response.status, 200,
                        "ordinary LFS {operation} failed at {path}"
                    );
                    let json: serde_json::Value =
                        serde_json::from_slice(&response.body).expect("ordinary LFS batch JSON");
                    assert_eq!(json["objects"], serde_json::json!([]));
                    assert_eq!(json["transfer"], "basic");
                    assert_eq!(json["hash_algo"], "sha256");
                    unchanged(&before, &snapshot(db).await);
                }
            }
        }

        async fn disabled_lfs(&self, db: &sea_orm::DatabaseConnection) {
            let before = snapshot(db).await;
            let canonical = request(
                self.port,
                Endpoint::Discovery,
                &format!("/.traces/by-scope/{}.git", self.scope),
                false,
                b"",
                None,
                false,
            )
            .await;
            canonical.refused(&self.tip);
            unchanged(&before, &snapshot(db).await);
            for prefix in ["%2etraces", ".%74races", ".traces"] {
                let path = format!(
                    "/{prefix}/by-scope/{}.git/info/lfs/objects/batch",
                    self.scope
                );
                for operation in ["download", "upload"] {
                    let body = lfs_batch(operation);
                    for authorization in [None, Some(SYNTHETIC_BASIC), Some(SYNTHETIC_BEARER)] {
                        let headers: Vec<_> = authorization
                            .map(|value| ("Authorization", value))
                            .into_iter()
                            .collect();
                        let response = raw_request(
                            self.port,
                            "POST",
                            &path,
                            Some(LFS_CONTENT_TYPE),
                            &body,
                            &headers,
                            false,
                        )
                        .await;
                        assert_eq!(
                            response.status, 404,
                            "disabled LFS {operation} alias bypass: {path}"
                        );
                        response.refused(&self.tip);
                        assert_eq!(
                            response.body, canonical.body,
                            "LFS alias must use the same disabled refusal"
                        );
                        unchanged(&before, &snapshot(db).await);
                    }
                }
                for (content_type, body, omit_body) in [
                    (LFS_CONTENT_TYPE, b"{".as_slice(), false),
                    ("text/plain", b"invalid LFS JSON".as_slice(), false),
                    (LFS_CONTENT_TYPE, b"".as_slice(), true),
                ] {
                    let response = raw_request(
                        self.port,
                        "POST",
                        &path,
                        Some(content_type),
                        body,
                        &[],
                        omit_body,
                    )
                    .await;
                    response.refused(&self.tip);
                    assert_eq!(
                        response.body, canonical.body,
                        "LFS body must not affect disabled refusal"
                    );
                    unchanged(&before, &snapshot(db).await);
                }
            }
        }

        async fn relocate_import(&mut self, db: &sea_orm::DatabaseConnection) {
            self.stop();
            let before = snapshot(db).await;
            let id = scalar(db, "SELECT id::text AS value FROM git_repo WHERE repo_path='/third-party/mtr94-control'").await;
            let path = format!("/%2etraces/by-scope/{}", self.scope);
            let result = db
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "UPDATE git_repo SET repo_path=$1 WHERE id::text=$2",
                    [path.into(), id.clone().into()],
                ))
                .await
                .expect("relocate import fixture");
            assert_eq!(result.rows_affected(), 1);
            let after = snapshot(db).await;
            let mut before_rest = before.clone();
            let mut after_rest = after.clone();
            before_rest.remove("git_repo");
            after_rest.remove("git_repo");
            unchanged(&before_rest, &after_rest);
            assert_eq!(scalar(db, &format!("SELECT id::text AS value FROM git_repo WHERE repo_path='/%2etraces/by-scope/{}'", self.scope)).await, id);
            self.start(true, "trunk", "/%2etraces").await;
            let url = git_cli::mega2_http_url(
                self.port,
                &format!("/%2etraces/by-scope/{}.git", self.scope),
            );
            self.git(&["clone", &url, "relocated-import-check"]);
            assert_eq!(
                fs::read(
                    self.shared
                        .path()
                        .join("relocated-import-check/sentinel.txt")
                )
                .expect("relocated import clone bytes before disabling traces"),
                SENTINEL
            );
            let tip = self.git_text(&["-C", "relocated-import-check", "rev-parse", "HEAD"]);
            let mut command = Vec::new();
            pkt(&mut command, &format!("want {tip}\n"));
            command.extend_from_slice(b"0000");
            pkt(&mut command, "done\n");
            let response = raw_request(
                self.port,
                "POST",
                &format!("/%2etraces/by-scope/{}.git/git-upload-pack", self.scope),
                Endpoint::Upload.content_type(),
                &http_git_gzip(&command),
                &[("Content-Encoding", "gzip")],
                false,
            )
            .await;
            assert_eq!(
                response.status, 200,
                "enabled literal alias keeps ordinary gzip decompression"
            );
            let pack = http_git_unpack_response(&response.body, false);
            let pack = pack
                .strip_suffix(b"0000")
                .expect("ordinary v0 response includes a trailing flush");
            fs::write(self.shared.path().join("alias-gzip.pack"), pack)
                .expect("ordinary gzip response PACK");
            self.git(&[
                "-C",
                "relocated-import-check",
                "index-pack",
                "../alias-gzip.pack",
            ]);
            self.start(false, "trunk", "/%2etraces").await;
            self.converge(db).await;
        }

        async fn enabled(&mut self, db: &sea_orm::DatabaseConnection, endpoint: Endpoint) {
            self.start(true, "trunk", "/%2etraces").await;
            self.converge(db).await;
            let before = snapshot(db).await;
            let locator = format!("/.traces/by-scope/{}.git", self.scope);
            for v2 in [false, true] {
                let response = request(
                    self.port,
                    endpoint,
                    &locator,
                    v2,
                    &self.request_body(endpoint, v2),
                    None,
                    false,
                )
                .await;
                assert_eq!(response.status, 401);
                assert!(
                    response
                        .headers
                        .to_ascii_lowercase()
                        .contains("www-authenticate:")
                );
                assert!(
                    response.body.is_empty(),
                    "anonymous traces must not advertise or decode"
                );
                unchanged(&before, &snapshot(db).await);
            }
            if matches!(endpoint, Endpoint::Receive) {
                let mut no_pack = Vec::new();
                pkt(
                    &mut no_pack,
                    &format!(
                        "{} {} refs/libra/traces\0report-status\n",
                        "0".repeat(40),
                        self.tip
                    ),
                );
                no_pack.extend_from_slice(b"0000");
                for body in [b"0000".as_slice(), self.pack.as_slice(), no_pack.as_slice()] {
                    let response =
                        request(self.port, endpoint, &locator, false, body, None, false).await;
                    assert_eq!(response.status, 401);
                    assert!(
                        response
                            .headers
                            .to_ascii_lowercase()
                            .contains("www-authenticate:")
                    );
                    assert!(response.body.is_empty());
                    unchanged(&before, &snapshot(db).await);
                }
                let output = git_cli::git_cli_no_auth(
                    self.shared.path(),
                    &[
                        "-C",
                        "import",
                        "push",
                        &git_cli::mega2_http_url(self.port, &locator),
                        "HEAD:refs/libra/traces",
                    ],
                );
                assert!(
                    !output.status.success(),
                    "anonymous trace push must be rejected"
                );
                let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                assert!(
                    error.contains("authentication")
                        || error.contains("401")
                        || error.contains("could not read username"),
                    "Git must report authentication refusal: {error}"
                );
                unchanged(&before, &snapshot(db).await);
            }
        }
    }

    struct SshClient;

    impl russh::client::Handler for SshClient {
        type Error = russh::Error;

        async fn check_server_key(
            &mut self,
            _key: &russh::keys::PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            // Only the isolated loopback fixture is contacted by this raw client.
            Ok(true)
        }
    }

    type SshSession = russh::client::Handle<SshClient>;

    impl Fixture {
        async fn seed_ssh(&self, db: &sea_orm::DatabaseConnection) {
            let key = self.shared.path().join("ssh/client_ed25519");
            git_cli::generate_client_ed25519(&key);
            let public = fs::read_to_string(key.with_extension("pub")).expect("SSH public key");
            let finger = git_cli::ssh_fingerprint_sha256_col2(&key.with_extension("pub"));
            let result = db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "INSERT INTO ssh_keys (id, username, title, ssh_key, finger, created_at) VALUES ($1, $2, $3, $4, $5, now())",
                [Utc::now().timestamp_micros().into(), git_cli::DEFAULT_SSH_AUTH_USER.into(),
                    "MTR100 disposable fixture".into(), public.into(), finger.into()],
            )).await.expect("seed real SSH key without a nested runtime");
            assert_eq!(result.rows_affected(), 1);
        }

        async fn start_ssh(&mut self, enabled: bool, storage_only: bool) {
            self.stop();
            self.port = git_cli::reserve_ephemeral_port();
            let policy = if storage_only { "trunk" } else { "review" };
            let mut command = self.command(enabled, policy, "/third-party");
            command.env("MEGA_GIT__SSH_RECEIVE_PACK", (!storage_only).to_string());
            command.env("MEGA_GIT_OBJECT_CACHE_PREFIX", "disabled");
            command.args([
                "service",
                "ssh",
                "--host",
                git_cli::service_listen_host(),
                "--ssh-port",
            ]);
            command.arg(self.port.to_string());
            self.service = OwnedChild::spawn(&mut command);
            git_cli::record_service_pid(self.service.0.as_ref().expect("SSH child").id());
            let deadline = Instant::now() + LIMIT;
            loop {
                assert!(
                    self.service
                        .0
                        .as_mut()
                        .expect("SSH child")
                        .try_wait()
                        .expect("poll SSH")
                        .is_none(),
                    "SSH service exited before readiness"
                );
                if let Ok(mut socket) =
                    tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await
                {
                    let mut banner = [0; 4];
                    if matches!(
                        tokio::time::timeout(
                            Duration::from_secs(2),
                            socket.read_exact(&mut banner)
                        )
                        .await,
                        Ok(Ok(_))
                    ) {
                        assert_eq!(&banner, b"SSH-", "real SSH listener banner");
                        break;
                    }
                }
                assert!(Instant::now() < deadline, "SSH readiness exceeded 90s");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            git_cli::write_known_hosts_via_keyscan(
                &self.shared.path().join("ssh/known_hosts"),
                self.port,
            );
        }

        async fn ssh_session(&self, storage_only: bool) -> SshSession {
            let (session,) = tokio::time::timeout(Duration::from_secs(10), async {
                let mut session = russh::client::connect(
                    std::sync::Arc::new(russh::client::Config::default()),
                    ("127.0.0.1", self.port),
                    SshClient,
                )
                .await
                .expect("connect real SSH fixture");
                let result = if storage_only {
                    session
                        .authenticate_none(git_cli::DEFAULT_SSH_AUTH_USER)
                        .await
                        .expect("storage-only anonymous SSH auth")
                } else {
                    let key = russh::keys::load_secret_key(
                        self.shared.path().join("ssh/client_ed25519"),
                        None,
                    )
                    .expect("SSH private key");
                    session
                        .authenticate_publickey(
                            git_cli::DEFAULT_SSH_AUTH_USER,
                            russh::keys::PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), None),
                        )
                        .await
                        .expect("real SSH public key authentication")
                };
                assert!(
                    result.success(),
                    "SSH oracle requires successful authentication"
                );
                (session,)
            })
            .await
            .expect("SSH authentication deadline");
            session
        }

        fn ssh_git(&self, args: &[&str]) -> std::process::Output {
            git_cli::git_cli_ssh(
                self.shared.path(),
                &git_cli::git_ssh_command(self.shared.path(), self.port),
                args,
            )
        }

        fn ssh_url(&self, path: &str) -> String {
            // Git decodes the SSH URL before sending the literal exec argument.
            let path = path.replace('%', "%25");
            format!(
                "ssh://{}@{}:{}{path}",
                git_cli::DEFAULT_SSH_AUTH_USER,
                git_cli::mega2_reachable_host(),
                self.port
            )
        }

        fn ssh_locators(&self) -> Vec<String> {
            // SSH exec arguments are literal paths, not HTTP percent-encoded URIs.
            let canonical = format!("/.traces/by-scope/{}.git", self.scope);
            let mut paths: Vec<_> = self
                .locators()
                .into_iter()
                .filter(|p| {
                    p.starts_with("/.traces/")
                        || p.starts_with("//.traces/")
                        || p.starts_with("/./.traces/")
                        || p.starts_with("/other/../.traces/")
                        || p == "/.traces.git"
                })
                .collect();
            paths.extend([
                canonical.trim_start_matches('/').to_owned(),
                format!("/other/../../{canonical}"),
                "/.traces/../third-party/mtr94-control.git".to_owned(),
                "/.traces".to_owned(),
            ]);
            paths
        }

        async fn ssh_ordinary(&self, db: &sea_orm::DatabaseConnection, session: &SshSession) {
            let before = snapshot(db).await;
            let paths = [
                "/project/control.git".to_owned(),
                "/project/.traces/repo.git".to_owned(),
                "/.traces-backup/repo.git".to_owned(),
                "/.tracesx/repo.git".to_owned(),
                "/.TRACES/repo.git".to_owned(),
                "/.traces.git/ordinary.git".to_owned(),
                format!("/%2etraces/by-scope/{}.git", self.scope),
                "/third-party/mtr94-control.git".to_owned(),
            ];
            for (index, path) in paths.iter().enumerate() {
                let output =
                    self.ssh_git(&["-c", "protocol.version=0", "ls-remote", &self.ssh_url(path)]);
                assert!(
                    output.status.success(),
                    "ordinary SSH {path}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("refs/heads/main"));
                let destination = format!("ssh-ordinary-{index}");
                let output = self.ssh_git(&["clone", &self.ssh_url(path), &destination]);
                assert!(
                    output.status.success(),
                    "ordinary SSH clone {path}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    fs::read(self.shared.path().join(destination).join("sentinel.txt"))
                        .expect("SSH clone sentinel"),
                    SENTINEL
                );
                unchanged(&before, &snapshot(db).await);
            }
            // The same authenticated connection remains usable after rejected channels.
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut channel = session
                    .channel_open_session()
                    .await
                    .expect("ordinary channel after rejection");
                channel
                    .exec(true, "git-upload-pack '/project/control.git'")
                    .await
                    .expect("ordinary exec");
                let mut advertised = Vec::new();
                loop {
                    match channel.wait().await.expect("ordinary advertisement") {
                        russh::ChannelMsg::Data { data } => {
                            advertised.extend_from_slice(&data);
                            assert!(
                                advertised.len() < 1024 * 1024,
                                "bounded ordinary advertisement"
                            );
                            if advertised.ends_with(b"0000") {
                                break;
                            }
                        }
                        russh::ChannelMsg::Failure | russh::ChannelMsg::Close => {
                            panic!("rejected channel poisoned ordinary exec")
                        }
                        _ => {}
                    }
                }
                assert!(String::from_utf8_lossy(&advertised).contains("refs/heads/main"));
                channel.eof().await.expect("finish ordinary discovery");
                channel.close().await.expect("close ordinary channel");
            })
            .await
            .expect("ordinary SSH discovery deadline");
            unchanged(&before, &snapshot(db).await);
        }

        async fn ssh_disabled(
            &self,
            db: &sea_orm::DatabaseConnection,
            session: &SshSession,
            receive: bool,
        ) {
            let service = if receive {
                "git-receive-pack"
            } else {
                "git-upload-pack"
            };
            for path in self.ssh_locators() {
                for v2 in [false, true] {
                    let before = snapshot(db).await;
                    // Exercise both supported quote forms through the real exec parser.
                    let command = if v2 {
                        format!("{service} \"{path}\"")
                    } else {
                        format!("{service} '{path}'")
                    };
                    ssh_refused(session, &command, v2).await;
                    unchanged(&before, &snapshot(db).await);
                }
            }
            let url = self.ssh_url(&format!("/.traces/by-scope/{}.git", self.scope));
            let before = snapshot(db).await;
            let output = if receive {
                self.ssh_git(&["-C", "import", "push", &url, "HEAD:refs/libra/traces"])
            } else {
                self.ssh_git(&["ls-remote", &url])
            };
            assert!(
                !output.status.success(),
                "real Git must reject disabled SSH traces"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("traces locator not found"),
                "not an authentication/connection failure: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            unchanged(&before, &snapshot(db).await);
        }

        async fn ssh_enabled(&mut self, db: &sea_orm::DatabaseConnection, receive: bool) {
            self.start_ssh(true, false).await;
            let session = self.ssh_session(false).await;
            let before = snapshot(db).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut channel = session
                    .channel_open_session()
                    .await
                    .expect("enabled SSH channel");
                let service = if receive {
                    "git-receive-pack"
                } else {
                    "git-upload-pack"
                };
                channel
                    .exec(
                        true,
                        format!("{service} '/.traces/by-scope/{}.git'", self.scope),
                    )
                    .await
                    .expect("enabled SSH exec");
                let mut advertisement = Vec::new();
                loop {
                    match channel
                        .wait()
                        .await
                        .expect("enabled existing advertisement")
                    {
                        russh::ChannelMsg::Data { data } => {
                            advertisement.extend_from_slice(&data);
                            assert!(
                                advertisement.len() < 1024 * 1024,
                                "bounded enabled advertisement"
                            );
                            if advertisement.ends_with(b"0000") {
                                break;
                            }
                        }
                        russh::ChannelMsg::Failure | russh::ChannelMsg::Close => {
                            panic!("enabled path was replaced by disabled refusal")
                        }
                        _ => {}
                    }
                }
                assert!(String::from_utf8_lossy(&advertisement).contains("capabilities^{}"));
                assert!(!String::from_utf8_lossy(&advertisement).contains(&self.tip));
                let endpoint = if receive {
                    Endpoint::Receive
                } else {
                    Endpoint::Upload
                };
                channel
                    .data(self.request_body(endpoint, false).as_slice())
                    .await
                    .expect("enabled real PACK/request");
                channel.eof().await.expect("finish enabled request");
                let mut response = Vec::new();
                while let Some(message) = channel.wait().await {
                    match message {
                        russh::ChannelMsg::Data { data }
                        | russh::ChannelMsg::ExtendedData { data, .. } => {
                            response.extend_from_slice(&data)
                        }
                        russh::ChannelMsg::Close => break,
                        _ => {}
                    }
                }
                assert!(!response.windows(4).any(|bytes| bytes == b"PACK"));
                assert!(!String::from_utf8_lossy(&response).contains("ok refs/libra/traces"));
                assert!(
                    String::from_utf8_lossy(&response)
                        .contains("trace repository does not write code objects"),
                    "enabled fail-closed response: {}",
                    String::from_utf8_lossy(&response)
                );
            })
            .await
            .expect("enabled SSH existing refusal deadline");
            unchanged(&before, &snapshot(db).await);
            session
                .disconnect(russh::Disconnect::ByApplication, "fixture complete", "en")
                .await
                .expect("disconnect enabled SSH");
        }

        async fn ssh_storage_only(&mut self, db: &sea_orm::DatabaseConnection, receive: bool) {
            self.start_ssh(false, true).await;
            let session = self.ssh_session(true).await;
            let before = snapshot(db).await;
            let service = if receive {
                "git-receive-pack"
            } else {
                "git-upload-pack"
            };
            ssh_refused(
                &session,
                &format!("{service} '/.traces/by-scope/{}.git'", self.scope),
                false,
            )
            .await;
            unchanged(&before, &snapshot(db).await);
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut channel = session
                    .channel_open_session()
                    .await
                    .expect("storage-only ordinary channel");
                channel
                    .exec(true, "git-receive-pack '/project/control.git'")
                    .await
                    .expect("storage-only exec");
                let mut stderr = Vec::new();
                let mut eof = false;
                let mut closed = false;
                while let Some(message) = channel.wait().await {
                    match message {
                        russh::ChannelMsg::ExtendedData { data, ext } => {
                            assert_eq!(ext, 1);
                            stderr.extend_from_slice(&data);
                        }
                        russh::ChannelMsg::Data { data } => {
                            assert!(data.is_empty(), "storage-only receive must not advertise")
                        }
                        russh::ChannelMsg::Eof => eof = true,
                        russh::ChannelMsg::Close => {
                            closed = true;
                            break;
                        }
                        russh::ChannelMsg::Failure => {
                            panic!("storage-only refusal must identify the existing gate")
                        }
                        _ => {}
                    }
                }
                assert_eq!(stderr, b"error: SSH receive-pack is disabled\n");
                assert!(eof && closed, "storage-only early refusal EOF and close");
            })
            .await
            .expect("storage-only receive refusal with stdin held open");
            unchanged(&before, &snapshot(db).await);
            session
                .disconnect(russh::Disconnect::ByApplication, "fixture complete", "en")
                .await
                .expect("disconnect storage-only SSH");
        }
    }

    async fn ssh_refused(session: &SshSession, command: &str, v2: bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut channel = session
                .channel_open_session()
                .await
                .expect("disabled SSH channel");
            if v2 {
                channel
                    .set_env(false, "GIT_PROTOCOL", "version=2")
                    .await
                    .expect("SSH protocol env");
            }
            channel
                .exec(true, command)
                .await
                .expect("disabled SSH exec");
            // Deliberately send neither data nor EOF: refusal must precede stdin consumption.
            let mut expected = Vec::new();
            pkt(&mut expected, "ERR traces locator not found\n");
            let mut actual = Vec::new();
            let mut status = None;
            let mut eof = false;
            let mut closed = false;
            let mut accepted = false;
            while let Some(message) = channel.wait().await {
                match message {
                    russh::ChannelMsg::Data { data } => {
                        actual.extend_from_slice(&data);
                        assert!(
                            expected.starts_with(&actual),
                            "disabled SSH leaked advertisement/fallback for {command}: {}",
                            String::from_utf8_lossy(&actual)
                        );
                    }
                    russh::ChannelMsg::ExtendedData { data, .. } => assert!(
                        data.is_empty(),
                        "expected traces ERR, not generic SSH refusal"
                    ),
                    russh::ChannelMsg::Success => accepted = true,
                    russh::ChannelMsg::Failure => {
                        panic!("exec failure is not the traces protocol oracle")
                    }
                    russh::ChannelMsg::ExitStatus { exit_status } => {
                        assert!(status.replace(exit_status).is_none());
                    }
                    russh::ChannelMsg::Eof => eof = true,
                    russh::ChannelMsg::Close => {
                        closed = true;
                        break;
                    }
                    _ => {}
                }
            }
            assert_eq!(actual, expected, "precise traces refusal for {command}");
            assert_eq!(status, Some(1), "traces refusal exit status");
            assert!(
                accepted && eof && closed,
                "exec success, EOF and close required for {command}"
            );
        })
        .await
        .expect("disabled SSH must refuse before stdin/EOF within 10s");
    }

    pub async fn run_ssh(receive: bool) {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(Fixture::new(&db).await);
            let fixture = fixture.as_mut().expect("SSH fixture");
            fixture.prepare(&db).await;
            fixture.seed_ssh(&db).await;
            fixture.start_ssh(false, false).await;
            let session = fixture.ssh_session(false).await;
            fixture.ssh_disabled(&db, &session, receive).await;
            fixture.ssh_ordinary(&db, &session).await;
            session
                .disconnect(russh::Disconnect::ByApplication, "fixture complete", "en")
                .await
                .expect("disconnect disabled SSH");
            fixture.ssh_enabled(&db, receive).await;
            fixture.ssh_storage_only(&db, receive).await;
        })
        .catch_unwind()
        .await;
        let cleanup = std::panic::catch_unwind(AssertUnwindSafe(|| {
            if let Some(fixture) = &mut fixture {
                fixture.stop();
            }
        }));
        let closed = db.close().await;
        drop(guard);
        if (result.is_err() || cleanup.is_err())
            && let Some(fixture) = fixture.take()
        {
            let logs = fixture.temp.keep();
            for name in ["process.out", "process.err"] {
                if let Ok(bytes) = fs::read(logs.join(name)) {
                    eprintln!(
                        "SSH fixture failure log retained: file={} sha256={}",
                        logs.join(name).display(),
                        hex::encode(Sha256::digest(bytes))
                    );
                }
            }
        }
        drop(fixture);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        if let Err(panic) = cleanup {
            std::panic::resume_unwind(panic);
        }
        closed.expect("close SSH fixture pool before database guard");
    }

    async fn scalar(db: &sea_orm::DatabaseConnection, sql: &str) -> String {
        db.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.to_owned(),
        ))
        .await
        .expect("fixture scalar query")
        .expect("fixture scalar row")
        .try_get("", "value")
        .expect("fixture scalar value")
    }

    async fn snapshot(db: &sea_orm::DatabaseConnection) -> Snapshot {
        let txn = db
            .begin_with_config(
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            )
            .await
            .expect("begin business snapshot");
        let tables = txn
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY tablename"
                    .to_owned(),
            ))
            .await
            .expect("list public business tables");
        let mut snapshot = BTreeMap::new();
        for row in tables {
            let table: String = row.try_get("", "tablename").expect("table name");
            let quoted = table.replace('"', "\"\"");
            let rows = txn.query_all_raw(Statement::from_string(DatabaseBackend::Postgres,
                format!("SELECT to_jsonb(t)::text AS row_data FROM public.\"{quoted}\" t ORDER BY to_jsonb(t)::text"))).await.expect("read business table rows");
            let values = rows
                .into_iter()
                .map(|row| row.try_get("", "row_data").expect("business row JSON"))
                .collect();
            snapshot.insert(table, values);
        }
        txn.commit().await.expect("finish business snapshot");
        snapshot
    }

    fn unchanged(before: &Snapshot, after: &Snapshot) {
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>(),
            "business table inventory changed"
        );
        for (table, rows) in before {
            if after.get(table) != Some(rows) {
                let old = Sha256::digest(serde_json::to_vec(rows).expect("snapshot JSON"));
                let new = Sha256::digest(serde_json::to_vec(&after[table]).expect("snapshot JSON"));
                panic!(
                    "business state changed: table={table} before={} after={}",
                    hex::encode(old),
                    hex::encode(new)
                );
            }
        }
    }

    fn pkt(out: &mut Vec<u8>, line: &str) {
        out.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
        out.extend_from_slice(line.as_bytes());
    }

    struct Response {
        status: u16,
        headers: String,
        body: Vec<u8>,
    }

    impl Response {
        fn refused(&self, oid: &str) {
            assert_eq!(self.status, 404, "disabled traces request must be refused");
            assert!(
                !self
                    .headers
                    .to_ascii_lowercase()
                    .contains("www-authenticate:")
            );
            assert!(!self.body.windows(4).any(|w| w == b"PACK"));
            assert!(!self.body.windows(oid.len()).any(|w| w == oid.as_bytes()));
            assert!(!self.body.windows(SENTINEL.len()).any(|w| w == SENTINEL));
            assert!(
                !self
                    .body
                    .windows(b"refs/heads/main".len())
                    .any(|w| w == b"refs/heads/main")
            );
        }
    }

    async fn request(
        port: u16,
        endpoint: Endpoint,
        locator: &str,
        v2: bool,
        body: &[u8],
        encoding: Option<&str>,
        omit_body: bool,
    ) -> Response {
        let mut headers = Vec::new();
        if v2 {
            headers.push(("Git-Protocol", "version=2"));
        }
        if let Some(encoding) = encoding {
            headers.push(("Content-Encoding", encoding));
        }
        raw_request(
            port,
            endpoint.method(),
            &format!("{locator}{}", endpoint.suffix()),
            endpoint.content_type(),
            body,
            &headers,
            omit_body,
        )
        .await
    }

    fn lfs_batch(operation: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "operation": operation, "transfers": ["basic"], "objects": [], "hash_algo": "sha256"
        }))
        .expect("LFS fixture JSON")
    }

    async fn raw_request(
        port: u16,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
        extra_headers: &[(&str, &str)],
        omit_body: bool,
    ) -> Response {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect raw HTTP fixture");
            let length = if omit_body { 4096 } else { body.len() };
            let mut headers = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {length}\r\n");
            for (name, value) in extra_headers {
                headers.push_str(&format!("{name}: {value}\r\n"));
            }
            if let Some(content_type) = content_type {
                headers.push_str(&format!("Content-Type: {content_type}\r\n"));
            }
            headers.push_str("\r\n");
            socket.write_all(headers.as_bytes()).await.expect("write request headers");
            if !omit_body { socket.write_all(body).await.expect("write request body"); }
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 1024];
                let n = socket.read(&mut chunk).await.expect("read HTTP response headers");
                assert_ne!(n, 0, "HTTP response ended before headers");
                bytes.extend_from_slice(&chunk[..n]);
                assert!(bytes.len() <= 64 * 1024, "bounded HTTP response headers");
                if let Some(position) = bytes.windows(4).position(|w| w == b"\r\n\r\n") { break position + 4; }
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("HTTP headers UTF8");
            let status = headers.split_whitespace().nth(1).expect("HTTP status").parse().expect("numeric status");
            let length: Option<usize> = headers.lines().find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("content-length")).map(|(_, value)| value.trim().parse().expect("response length")));
            let mut body = bytes[header_end..].to_vec();
            if let Some(length) = length {
                assert!(length < 1024 * 1024, "bounded fixture response");
                while body.len() < length {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.expect("read response body");
                    assert_ne!(n, 0, "truncated HTTP response body");
                    body.extend_from_slice(&chunk[..n]);
                }
                assert_eq!(body.len(), length);
            } else {
                let mut tail = Vec::new();
                socket.take(1024 * 1024).read_to_end(&mut tail).await.expect("read bounded response");
                body.extend_from_slice(&tail);
                assert!(body.len() < 1024 * 1024);
                if headers.to_ascii_lowercase().contains("transfer-encoding: chunked") {
                    let mut decoded = Vec::new();
                    let mut remaining = body.as_slice();
                    loop {
                        let end = remaining.windows(2).position(|w| w == b"\r\n").expect("chunk size line");
                        let size = usize::from_str_radix(std::str::from_utf8(&remaining[..end]).expect("chunk size").split(';').next().expect("size"), 16).expect("hex chunk size");
                        remaining = &remaining[end + 2..];
                        if size == 0 { break; }
                        assert!(remaining.len() >= size + 2, "complete chunk");
                        decoded.extend_from_slice(&remaining[..size]);
                        assert_eq!(&remaining[size..size + 2], b"\r\n");
                        remaining = &remaining[size + 2..];
                    }
                    body = decoded;
                }
            }
            Response { status, headers, body }
        }).await.expect("raw HTTP response deadline (body deliberately omitted when requested)")
    }

    fn http_git_commit(fixture: &Fixture, message: &str) -> String {
        fixture.git(&["-C", "trace-source", "add", "."]);
        fixture.git(&[
            "-C",
            "trace-source",
            "-c",
            "user.name=HTTP Traces",
            "-c",
            "user.email=http@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            message,
        ]);
        fixture.git_text(&["-C", "trace-source", "rev-parse", "HEAD"])
    }

    fn http_git_gzip(bytes: &[u8]) -> Vec<u8> {
        let zlib =
            git_internal::internal::object::utils::compress_zlib(bytes).expect("fixture deflate");
        assert!(
            zlib.len() >= 6 && zlib[1] & 0x20 == 0,
            "zlib without dictionary"
        );
        let mut gzip = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        gzip.extend_from_slice(&zlib[2..zlib.len() - 4]);
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320u32 & 0u32.wrapping_sub(crc & 1));
            }
        }
        gzip.extend_from_slice(&(!crc).to_le_bytes());
        gzip.extend_from_slice(
            &u32::try_from(bytes.len())
                .expect("fixture length")
                .to_le_bytes(),
        );
        gzip
    }

    fn http_git_rejected_history(push: &std::process::Output) {
        assert!(
            !push.status.success(),
            "checkpoint history rewrite must reject the whole push"
        );
        assert!(
            String::from_utf8_lossy(&push.stderr).contains("The requested URL returned error: 409"),
            "history rejection must be the typed HTTP conflict: {}",
            String::from_utf8_lossy(&push.stderr)
        );
    }

    fn http_git_authenticated(
        fixture: &Fixture,
        token: &str,
        args: &[&str],
    ) -> std::process::Output {
        git_cli::git_cli(fixture.shared.path(), token, args)
    }

    async fn http_git_body(
        fixture: &Fixture,
        locator: &str,
        token: &str,
        v2: bool,
        body: &[u8],
    ) -> Response {
        let authorization = format!("Bearer {token}");
        let mut headers = vec![("Authorization", authorization.as_str())];
        if v2 {
            headers.push(("Git-Protocol", "version=2"));
        }
        raw_request(
            fixture.port,
            "POST",
            &format!("{locator}/git-upload-pack"),
            Endpoint::Upload.content_type(),
            body,
            &headers,
            false,
        )
        .await
    }

    fn http_git_unpack_response(body: &[u8], v2: bool) -> Vec<u8> {
        let prefix = if v2 {
            b"000dpackfile\n".as_slice()
        } else {
            b"0008NAK\n".as_slice()
        };
        assert!(
            body.starts_with(prefix),
            "standard acknowledgment/section header"
        );
        let mut rest = &body[prefix.len()..];
        if !v2 {
            return rest.to_vec();
        }
        let mut pack = Vec::new();
        while rest != b"0000" {
            assert!(rest.len() >= 5, "complete v2 band packet");
            let length =
                usize::from_str_radix(std::str::from_utf8(&rest[..4]).expect("band header"), 16)
                    .expect("band length");
            assert!((5..=65520).contains(&length) && length <= rest.len());
            assert_eq!(rest[4], 1, "only PACK band 1");
            pack.extend_from_slice(&rest[5..length]);
            rest = &rest[length..];
        }
        pack
    }

    async fn http_git_failed_stream(
        fixture: &Fixture,
        locator: &str,
        token: &str,
        tip: &str,
        first: &str,
    ) {
        use futures::StreamExt;

        let client = reqwest::Client::builder()
            .timeout(LIMIT)
            .build()
            .expect("stream failure client");
        for v2 in [false, true] {
            let mut command = Vec::new();
            if v2 {
                pkt(&mut command, "command=fetch\n");
                pkt(&mut command, "object-format=sha1\n");
                command.extend_from_slice(b"0001");
                pkt(&mut command, &format!("want {tip}\n"));
            } else {
                pkt(&mut command, &format!("want {tip} object-format=sha1\n"));
                command.extend_from_slice(b"0000");
            }
            pkt(&mut command, "done\n");
            if v2 {
                command.extend_from_slice(b"0000");
            }
            let response = client
                .post(git_cli::mega2_host_http_url(
                    fixture.port,
                    &format!("{locator}/git-upload-pack"),
                ))
                .bearer_auth(token)
                .header("Git-Protocol", if v2 { "version=2" } else { "version=0" })
                .header("Content-Type", "application/x-git-upload-pack-request")
                .body(command.clone())
                .send()
                .await
                .expect("fetch headers before reading its large body");
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let overloaded = http_git_body(fixture, locator, token, v2, &command).await;
            assert_eq!(
                overloaded.status, 503,
                "in-flight body retains the sole fetch permit"
            );
            let spools: Vec<_> = fs::read_dir(fixture.temp.path().join("owned-spool"))
                .expect("owned spools")
                .map(|entry| entry.expect("owned entry").path())
                .filter(|path| {
                    path.file_name()
                        .expect("name")
                        .to_string_lossy()
                        .starts_with("mega2-traces-fetch-")
                })
                .collect();
            assert_eq!(
                spools.len(),
                1,
                "exactly this response owns an unfinished spool"
            );
            let mut removed = 0;
            for entry in fs::read_dir(&spools[0]).expect("owned response objects") {
                let payload = entry.expect("object entry").path().join("payload");
                if payload.is_file() {
                    fs::remove_file(payload).expect("inject loss only in the owned response spool");
                    removed += 1;
                }
            }
            assert!(
                removed > 1,
                "fault injection removed response-owned object files"
            );
            let mut body = response.bytes_stream();
            let mut received = Vec::new();
            let mut failed = false;
            while let Some(chunk) = body.next().await {
                match chunk {
                    Ok(chunk) => received.extend_from_slice(&chunk),
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            assert!(
                failed,
                "source failure must abort HTTP body, not produce successful EOF"
            );
            drop(body);
            let prefix = if v2 {
                b"000dpackfile\n".as_slice()
            } else {
                b"0008NAK\n".as_slice()
            };
            assert!(received.starts_with(prefix));
            let mut rest = &received[prefix.len()..];
            let mut pack = Vec::new();
            if v2 {
                while rest.len() >= 4 {
                    let size = usize::from_str_radix(
                        std::str::from_utf8(&rest[..4]).expect("band header"),
                        16,
                    )
                    .expect("packet size");
                    assert_ne!(size, 0, "failed response must not emit a successful flush");
                    assert!((5..=65520).contains(&size));
                    if rest.len() < size {
                        break;
                    }
                    assert_eq!(rest[4], 1);
                    pack.extend_from_slice(&rest[5..size]);
                    rest = &rest[size..];
                }
            } else {
                pack.extend_from_slice(rest);
            }
            fs::write(fixture.shared.path().join("interrupted.pack"), pack)
                .expect("interrupted wire bytes");
            let index = git_cli::git_cli_no_auth(
                fixture.shared.path(),
                &["-C", "trace-source", "index-pack", "../interrupted.pack"],
            );
            assert!(
                !index.status.success(),
                "interrupted stream must not contain a valid complete PACK"
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while spools[0].exists() {
                assert!(
                    Instant::now() < deadline,
                    "failed response spool was not released"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let mut probe = Vec::new();
            pkt(&mut probe, &format!("want {first} object-format=sha1\n"));
            probe.extend_from_slice(b"0000");
            pkt(&mut probe, "done\n");
            loop {
                let response = http_git_body(fixture, locator, token, false, &probe).await;
                if response.status == 200 {
                    assert!(http_git_unpack_response(&response.body, false).starts_with(b"PACK"));
                    break;
                }
                assert_eq!(response.status, 503);
                assert!(
                    Instant::now() < deadline,
                    "failed response permit was not released"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    async fn http_git_roundtrip_inner(
        fixture: &mut Fixture,
        db: &sea_orm::DatabaseConnection,
        kind: &str,
    ) {
        let mut init = fixture.command(false, "trunk", "/third-party");
        init.args(["service", "init", "--yes"]);
        OwnedChild::spawn(&mut init).require_success().await;
        let token = format!("mtr11-synthetic-{}", Uuid::new_v4());
        mega2_core::access_token::ActiveModel {
            id: Set(1001),
            username: Set("mega2".to_owned()),
            token: Set(token.clone()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(db)
        .await
        .expect("seed fixture token using the existing async connection");
        let token_id = mega2_core::access_token::Entity::find()
            .filter(mega2_core::access_token::Column::Token.eq(&token))
            .one(db)
            .await
            .expect("seeded token")
            .expect("one token")
            .id;
        let principal = Uuid::new_v4().to_string();
        trace_principal::ActiveModel {
            id: Set(principal.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("principal fixture");
        trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal.clone()),
            namespace: Set("mono_account".to_owned()),
            credential_id: Set(format!("access-token:{token_id}")),
            created_at: Set(Utc::now().fixed_offset()),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .expect("credential fixture binding");
        fixture.traces_auth = true;
        fixture.start(true, "review", "/third-party").await;
        let locator = format!(
            "/.traces/by-path/{}/self/{}/{}/{kind}.git",
            URL_SAFE_NO_PAD.encode(b"/project"),
            URL_SAFE_NO_PAD.encode(b"http-source"),
            fixture.epoch
        );
        let unknown = format!("/.traces/by-scope/{}.git", Uuid::new_v4());
        let before = snapshot(db).await;
        for path in [&locator, &unknown] {
            for endpoint in [
                Endpoint::Discovery,
                Endpoint::ReceiveDiscovery,
                Endpoint::Upload,
                Endpoint::Receive,
            ] {
                let response = request(
                    fixture.port,
                    endpoint,
                    path,
                    false,
                    b"invalid PACK",
                    Some("gzip"),
                    true,
                )
                .await;
                assert_eq!(
                    response.status, 401,
                    "authenticate before body/decompression/lookup"
                );
                assert!(
                    response
                        .headers
                        .to_ascii_lowercase()
                        .contains("www-authenticate:")
                );
                assert!(response.body.is_empty());
            }
        }
        unchanged(&before, &snapshot(db).await);
        let authorization = format!("Bearer {token}");
        let headers = [("Authorization", authorization.as_str())];
        let unbound = format!("mtr11-unbound-{}", Uuid::new_v4());
        mega2_core::access_token::ActiveModel {
            id: Set(1002),
            username: Set("mega2".to_owned()),
            token: Set(unbound.clone()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(db)
        .await
        .expect("seed unbound token using the existing async connection");
        let unbound_authorization = format!("Bearer {unbound}");
        let mut denied_bodies = Vec::new();
        for path in [&locator, &unknown] {
            let invalid = raw_request(
                fixture.port,
                "POST",
                &format!("{path}/git-receive-pack"),
                Endpoint::Receive.content_type(),
                b"",
                &[("Authorization", SYNTHETIC_BEARER)],
                true,
            )
            .await;
            assert_eq!(invalid.status, 401);
            assert!(
                invalid
                    .headers
                    .to_ascii_lowercase()
                    .contains("www-authenticate:")
            );
            let denied = raw_request(
                fixture.port,
                "GET",
                &format!("{path}/info/refs?service=git-upload-pack"),
                None,
                b"",
                &[("Authorization", unbound_authorization.as_str())],
                false,
            )
            .await;
            assert_eq!(denied.status, 404);
            denied_bodies.push(denied.body);
        }
        assert_eq!(
            denied_bodies[0], denied_bodies[1],
            "valid unbound credentials cannot distinguish scope existence"
        );
        let before = snapshot(db).await;
        let probe = raw_request(
            fixture.port,
            "POST",
            &format!("{locator}/git-receive-pack"),
            Endpoint::Receive.content_type(),
            b"0000",
            &headers,
            false,
        )
        .await;
        assert_eq!(probe.status, 200);
        assert!(probe.body.is_empty());
        unchanged(&before, &snapshot(db).await);

        fixture.git(&[
            "init",
            "--initial-branch=main",
            &format!("--object-format={kind}"),
            "trace-source",
        ]);
        fixture.git(&["-C", "trace-source", "config", "pack.window", "0"]);
        fixture.git(&["-C", "trace-source", "config", "gc.auto", "0"]);
        let checkpoint = fixture.shared.path().join("trace-source/checkpoint/ab/cd");
        fs::create_dir_all(&checkpoint).expect("checkpoint fixture");
        let metadata = b"{\"schema_version\":2}\n";
        fs::write(checkpoint.join("metadata.json"), metadata).expect("checkpoint metadata");
        fs::write(checkpoint.join("raw.bin"), b"raw\0bytes\xff\n").expect("non-text raw blob");
        let first = http_git_commit(fixture, "first checkpoint");
        let url = git_cli::mega2_http_url(fixture.port, &locator);
        let push = http_git_authenticated(
            fixture,
            &token,
            &[
                "-C",
                "trace-source",
                "-c",
                "protocol.version=0",
                "push",
                &url,
                "HEAD:refs/libra/traces",
            ],
        );
        assert!(
            push.status.success(),
            "standard Git first push: {}",
            String::from_utf8_lossy(&push.stderr)
        );
        let stream = trace_stream::Entity::find()
            .filter(trace_stream::Column::EpochId.eq(&fixture.epoch))
            .one(db)
            .await
            .expect("new stream")
            .expect("first push created scope");
        assert_eq!(stream.tip_oid.as_deref(), Some(first.as_str()));
        assert_eq!(stream.hash_kind, kind);
        let by_scope = format!("/.traces/by-scope/{}.git", stream.id);
        let outsider = Uuid::new_v4().to_string();
        trace_principal::ActiveModel {
            id: Set(outsider.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("different bound principal");
        trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(outsider),
            namespace: Set("mono_account".to_owned()),
            credential_id: Set("access-token:1002".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .expect("bind outsider token");
        let before = snapshot(db).await;
        for endpoint in [Endpoint::ReceiveDiscovery, Endpoint::Receive] {
            let mut bodies = Vec::new();
            for path in [&by_scope, &unknown] {
                let denied = raw_request(
                    fixture.port,
                    endpoint.method(),
                    &format!("{path}{}", endpoint.suffix()),
                    endpoint.content_type(),
                    b"",
                    &[("Authorization", unbound_authorization.as_str())],
                    true,
                )
                .await;
                assert_eq!(
                    denied.status, 404,
                    "non-owner writes are denied before reading the body"
                );
                bodies.push(denied.body);
            }
            assert_eq!(
                bodies[0], bodies[1],
                "unauthorized existing scope is indistinguishable from unknown"
            );
        }
        let mut fetch_command = Vec::new();
        pkt(
            &mut fetch_command,
            &format!("want {first} object-format={kind}\n"),
        );
        fetch_command.extend_from_slice(b"0000");
        pkt(&mut fetch_command, "done\n");
        let plain = http_git_body(fixture, &by_scope, &token, false, &fetch_command).await;
        assert_eq!(plain.status, 200);
        let shared_read = raw_request(
            fixture.port,
            "GET",
            &format!("{by_scope}/info/refs?service=git-upload-pack"),
            None,
            b"",
            &[("Authorization", unbound_authorization.as_str())],
            false,
        )
        .await;
        assert_eq!(
            shared_read.status, 200,
            "project read permission includes another principal's active traces"
        );
        assert!(
            shared_read
                .body
                .windows(first.len())
                .any(|part| part == first.as_bytes())
        );
        let shared_fetch = http_git_body(fixture, &by_scope, &unbound, false, &fetch_command).await;
        assert_eq!(shared_fetch.status, 200);
        assert_eq!(
            shared_fetch.body, plain.body,
            "project-authorized readers receive the same raw objects"
        );
        let compressed = raw_request(
            fixture.port,
            "POST",
            &format!("{by_scope}/git-upload-pack"),
            Endpoint::Upload.content_type(),
            &http_git_gzip(&fetch_command),
            &[
                ("Authorization", authorization.as_str()),
                ("Content-Encoding", "gzip"),
            ],
            false,
        )
        .await;
        assert_eq!(compressed.status, 200, "authenticated gzip fetch");
        assert_eq!(
            compressed.body, plain.body,
            "gzip and identity preserve exact PACK bytes"
        );
        let mut random = vec![0; 64 * 1024];
        let mut state = 0x4ca1_a784_5633_4acdu64;
        for byte in &mut random {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let encoded_overflow = http_git_gzip(&random);
        assert!(encoded_overflow.len() > 64 * 1024 && random.len() == 64 * 1024);
        let decoded_overflow = http_git_gzip(&vec![b'x'; 64 * 1024 + 1]);
        assert!(decoded_overflow.len() < 64 * 1024);
        for (endpoint, encoding, body, status, code) in [
            (
                Endpoint::Upload,
                "br",
                fetch_command.clone(),
                400,
                "invalid_parameter",
            ),
            (
                Endpoint::Upload,
                "gzip",
                b"malformed gzip".to_vec(),
                400,
                "invalid_parameter",
            ),
            (
                Endpoint::Upload,
                "gzip",
                encoded_overflow,
                413,
                "response_budget",
            ),
            (
                Endpoint::Upload,
                "gzip",
                decoded_overflow,
                413,
                "response_budget",
            ),
            (
                Endpoint::Upload,
                "identity",
                vec![b'x'; 64 * 1024 + 1],
                413,
                "response_budget",
            ),
            (
                Endpoint::Receive,
                "gzip",
                http_git_gzip(b"0000"),
                400,
                "invalid_parameter",
            ),
        ] {
            let response = raw_request(
                fixture.port,
                "POST",
                &format!("{by_scope}{}", endpoint.suffix()),
                endpoint.content_type(),
                &body,
                &[
                    ("Authorization", authorization.as_str()),
                    ("Content-Encoding", encoding),
                ],
                false,
            )
            .await;
            assert_eq!(
                response.status, status,
                "encoding {encoding}, expected {code}"
            );
            assert!(
                response
                    .body
                    .windows(code.len())
                    .any(|part| part == code.as_bytes())
            );
        }
        unchanged(&before, &snapshot(db).await);
        for (index, path) in [&locator, &by_scope].into_iter().enumerate() {
            for version in ["0", "2"] {
                let remote = git_cli::mega2_http_url(fixture.port, path);
                let refs = http_git_authenticated(
                    fixture,
                    &token,
                    &[
                        "-c",
                        &format!("protocol.version={version}"),
                        "ls-remote",
                        &remote,
                    ],
                );
                assert!(
                    refs.status.success(),
                    "ls-remote: {}",
                    String::from_utf8_lossy(&refs.stderr)
                );
                assert_eq!(
                    String::from_utf8(refs.stdout).expect("refs"),
                    format!("{first}\trefs/libra/traces\n")
                );
                let destination = format!("fetch-{index}-{version}");
                fixture.git(&["init", &format!("--object-format={kind}"), &destination]);
                let fetch = http_git_authenticated(
                    fixture,
                    &token,
                    &[
                        "-C",
                        &destination,
                        "-c",
                        &format!("protocol.version={version}"),
                        "fetch",
                        "--no-tags",
                        &remote,
                        "refs/libra/traces:refs/libra/traces",
                    ],
                );
                assert!(
                    fetch.status.success(),
                    "standard Git fetch: {}",
                    String::from_utf8_lossy(&fetch.stderr)
                );
                assert_eq!(
                    fixture.git_text(&["-C", &destination, "rev-parse", "refs/libra/traces"]),
                    first
                );
            }
        }
        for v2 in [false, true] {
            let mut command = Vec::new();
            if v2 {
                pkt(&mut command, "command=fetch\n");
                pkt(&mut command, &format!("object-format={kind}\n"));
                command.extend_from_slice(b"0001");
                for option in ["thin-pack", "ofs-delta", "no-progress", "include-tag"] {
                    pkt(&mut command, &format!("{option}\n"));
                }
                pkt(&mut command, &format!("want {first}\n"));
            } else if kind == "sha256" {
                pkt(&mut command, &format!("want {first}\n"));
                command.extend_from_slice(b"0000");
            } else {
                pkt(
                    &mut command,
                    &format!("want {first} object-format={kind}\n"),
                );
                command.extend_from_slice(b"0000");
            }
            pkt(&mut command, "done\n");
            if v2 {
                command.extend_from_slice(b"0000");
            }
            let response = http_git_body(fixture, &by_scope, &token, v2, &command).await;
            assert_eq!(response.status, 200);
            let pack = http_git_unpack_response(&response.body, v2);
            assert_eq!(&pack[..8], b"PACK\0\0\0\x02");
            let trailer = if kind == "sha1" { 20 } else { 32 };
            let payload = &pack[..pack.len() - trailer];
            let digest = if kind == "sha1" {
                sha1::Sha1::digest(payload).to_vec()
            } else {
                Sha256::digest(payload).to_vec()
            };
            assert_eq!(
                digest,
                pack[pack.len() - trailer..],
                "independent PACK checksum"
            );
            let dir = format!("raw-{v2}");
            fixture.git(&["init", &format!("--object-format={kind}"), &dir]);
            let pack_path = format!("{dir}/.git/objects/pack/http.pack");
            fs::write(fixture.shared.path().join(&pack_path), pack).expect("raw PACK oracle input");
            fixture.git(&["-C", &dir, "index-pack", ".git/objects/pack/http.pack"]);
            let oids = fixture.git_text(&["-C", "trace-source", "rev-list", "--objects", "--all"]);
            for line in oids.lines() {
                let oid = line.split_whitespace().next().expect("OID");
                let object_type = fixture.git_text(&["-C", "trace-source", "cat-file", "-t", oid]);
                assert_eq!(
                    fixture.git(&["-C", &dir, "cat-file", &object_type, oid]),
                    fixture.git(&["-C", "trace-source", "cat-file", &object_type, oid]),
                    "raw object {oid}"
                );
            }
            let denied_oid = "f".repeat(first.len());
            let denied = String::from_utf8(command)
                .expect("command text")
                .replace(&first, &denied_oid);
            let denied = http_git_body(fixture, &by_scope, &token, v2, denied.as_bytes()).await;
            assert_eq!(denied.status, 404, "unpublished want denied");
            assert!(!denied.body.windows(4).any(|part| part == b"PACK"));
        }

        if kind == "sha256" {
            let before_invalid = snapshot(db).await;
            for capability in [
                "object-format=sha1",
                "object-format=sha256 object-format=sha256",
                "server-option=unknown",
            ] {
                let mut command = Vec::new();
                pkt(&mut command, &format!("want {first} {capability}\n"));
                command.extend_from_slice(b"0000");
                pkt(&mut command, "done\n");
                let rejected = http_git_body(fixture, &by_scope, &token, false, &command).await;
                assert_eq!(rejected.status, 400, "v0 capability {capability}");
                assert!(
                    rejected
                        .body
                        .windows(17)
                        .any(|part| part == b"invalid_parameter")
                );
            }
            let mut short_oid = Vec::new();
            pkt(&mut short_oid, &format!("want {}\n", &first[..40]));
            short_oid.extend_from_slice(b"0000");
            pkt(&mut short_oid, "done\n");
            let rejected = http_git_body(fixture, &by_scope, &token, false, &short_oid).await;
            assert_eq!(rejected.status, 400, "v0 SHA-256 OID width");
            assert!(
                rejected
                    .body
                    .windows(17)
                    .any(|part| part == b"invalid_parameter")
            );

            let mut v2_missing_format = Vec::new();
            pkt(&mut v2_missing_format, "command=fetch\n");
            v2_missing_format.extend_from_slice(b"0001");
            pkt(&mut v2_missing_format, &format!("want {first}\n"));
            pkt(&mut v2_missing_format, "done\n");
            v2_missing_format.extend_from_slice(b"0000");
            let rejected =
                http_git_body(fixture, &by_scope, &token, true, &v2_missing_format).await;
            assert_eq!(rejected.status, 400, "v2 SHA-256 format remains required");
            assert!(
                rejected
                    .body
                    .windows(17)
                    .any(|part| part == b"invalid_parameter")
            );

            let mut receive_missing_format = Vec::new();
            pkt(
                &mut receive_missing_format,
                &format!("{first} {first} refs/libra/traces\0report-status\n"),
            );
            receive_missing_format.extend_from_slice(b"0000");
            let rejected = raw_request(
                fixture.port,
                "POST",
                &format!("{by_scope}/git-receive-pack"),
                Endpoint::Receive.content_type(),
                &receive_missing_format,
                &headers,
                false,
            )
            .await;
            assert_eq!(
                rejected.status, 400,
                "receive SHA-256 format remains required"
            );
            assert!(
                rejected
                    .body
                    .windows(17)
                    .any(|part| part == b"invalid_parameter")
            );
            unchanged(&before_invalid, &snapshot(db).await);
        }

        let second_checkpoint = fixture.shared.path().join("trace-source/checkpoint/ef/01");
        fs::create_dir_all(&second_checkpoint).expect("second checkpoint");
        fs::write(second_checkpoint.join("metadata.json"), metadata).expect("second metadata");
        let second = http_git_commit(fixture, "preserve and append checkpoint");
        let by_scope_url = git_cli::mega2_http_url(fixture.port, &by_scope);
        let push = http_git_authenticated(
            fixture,
            &token,
            &[
                "-C",
                "trace-source",
                "push",
                &by_scope_url,
                "HEAD:refs/libra/traces",
            ],
        );
        assert!(
            push.status.success(),
            "second push: {}",
            String::from_utf8_lossy(&push.stderr)
        );
        assert_eq!(
            trace_stream::Entity::find_by_id(&stream.id)
                .one(db)
                .await
                .expect("stream")
                .expect("scope")
                .tip_oid
                .as_deref(),
            Some(second.as_str())
        );
        let second = if kind == "sha1" {
            #[cfg(target_os = "linux")]
            {
                let maximum = |name: &str| -> usize {
                    fs::read_to_string(format!("/proc/sys/net/ipv4/{name}"))
                        .expect("socket buffer qualification")
                        .split_whitespace()
                        .last()
                        .expect("maximum socket buffer")
                        .parse()
                        .expect("numeric buffer size")
                };
                assert!(
                    maximum("tcp_rmem") + maximum("tcp_wmem") + 8 * 1024 * 1024 < 64 * 1024 * 1024,
                    "64MiB fixture must exceed qualified socket buffers plus application buffering"
                );
            }
            let mut state = 0x1234_5678_9abc_def0u64;
            for index in 0..128 {
                let mut bytes = vec![0; 512 * 1024];
                for byte in &mut bytes {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = state as u8;
                }
                fs::write(
                    fixture
                        .shared
                        .path()
                        .join(format!("trace-source/stream-{index:02}.bin")),
                    bytes,
                )
                .expect("backpressure fixture");
            }
            let large_tip = http_git_commit(fixture, "large pull-driven stream fixture");
            let push = http_git_authenticated(
                fixture,
                &token,
                &["-C", "trace-source", "push", &url, "HEAD:refs/libra/traces"],
            );
            assert!(
                push.status.success(),
                "large stream fixture push: {}",
                String::from_utf8_lossy(&push.stderr)
            );
            let before = snapshot(db).await;
            http_git_failed_stream(fixture, &by_scope, &token, &large_tip, &first).await;
            unchanged(&before, &snapshot(db).await);
            large_tip
        } else {
            second
        };
        let before = snapshot(db).await;
        fs::write(
            checkpoint.join("metadata.json"),
            b"{\"schema_version\":2,\"mutation\":true}\n",
        )
        .expect("intermediate mutation");
        http_git_commit(fixture, "mutate existing checkpoint");
        fs::write(checkpoint.join("metadata.json"), metadata).expect("restore final bytes");
        http_git_commit(fixture, "restore checkpoint at final tip");
        let push = http_git_authenticated(
            fixture,
            &token,
            &["-C", "trace-source", "push", &url, "HEAD:refs/libra/traces"],
        );
        http_git_rejected_history(&push);
        unchanged(&before, &snapshot(db).await);

        for (new_checkpoint, remove) in [(false, true), (true, false), (true, true)] {
            fixture.git(&["-C", "trace-source", "switch", "--detach", &second]);
            let leaf = if new_checkpoint {
                fixture.shared.path().join("trace-source/checkpoint/aa/bb")
            } else {
                checkpoint.clone()
            };
            if new_checkpoint {
                fs::create_dir_all(&leaf).expect("new checkpoint in same push");
                fs::write(leaf.join("metadata.json"), metadata).expect("same-push metadata");
                http_git_commit(fixture, "introduce checkpoint in batch");
            }
            let original = fs::read(leaf.join("metadata.json")).expect("original metadata");
            if remove {
                fs::remove_file(leaf.join("metadata.json")).expect("intermediate removal");
            } else {
                fs::write(
                    leaf.join("metadata.json"),
                    b"{\"schema_version\":2,\"mutation\":true}",
                )
                .expect("intermediate edit");
            }
            http_git_commit(fixture, "intermediate checkpoint change");
            fs::create_dir_all(&leaf).expect("restore checkpoint directory");
            fs::write(leaf.join("metadata.json"), &original).expect("restore checkpoint metadata");
            http_git_commit(fixture, "restore final checkpoint bytes");
            let push = http_git_authenticated(
                fixture,
                &token,
                &["-C", "trace-source", "push", &url, "HEAD:refs/libra/traces"],
            );
            http_git_rejected_history(&push);
            unchanged(&before, &snapshot(db).await);
        }

        fixture.git(&["-C", "trace-source", "switch", "--detach", &second]);
        let repeated = b"standard Git delta candidate payload\n".repeat(4096);
        fs::write(
            fixture.shared.path().join("trace-source/delta-a.bin"),
            &repeated,
        )
        .expect("delta base");
        let mut similar = repeated.clone();
        similar[1024] ^= 1;
        fs::write(
            fixture.shared.path().join("trace-source/delta-b.bin"),
            &similar,
        )
        .expect("delta target");
        let delta_tip = http_git_commit(fixture, "default delta negative fixture");
        fs::write(
            fixture
                .shared
                .path()
                .join("trace-source/.git/mtr11-pack-revisions"),
            format!("{delta_tip}\n^{second}\n"),
        )
        .expect("pack only new objects relative to the published tip");
        let pack_alias = "alias.mtr11-pack=!git pack-objects --stdout --revs --no-reuse-delta --no-reuse-object < .git/mtr11-pack-revisions";
        fixture.git(&["-C", "trace-source", "config", "--unset", "pack.window"]);
        let delta_pack = fixture.git(&["-C", "trace-source", "-c", pack_alias, "mtr11-pack"]);
        assert!(
            delta_pack.len() < 512 * 1024,
            "early delta rejection must not block a large request writer"
        );
        fixture.git(&["-C", "trace-source", "config", "pack.window", "0"]);
        fs::write(fixture.shared.path().join("delta.pack"), &delta_pack)
            .expect("actual Git delta PACK");
        fixture.git(&["-C", "trace-source", "index-pack", "../delta.pack"]);
        let inspected =
            fixture.git_text(&["-C", "trace-source", "verify-pack", "-v", "../delta.idx"]);
        assert!(
            inspected.lines().any(|line| {
                let columns: Vec<_> = line.split_whitespace().collect();
                columns.len() == 7
                    && columns[0].len() == first.len()
                    && columns[5].parse::<u32>().is_ok_and(|depth| depth > 0)
            }),
            "negative fixture must actually contain a delta"
        );
        let mut receive = Vec::new();
        pkt(
            &mut receive,
            &format!(
                "{second} {delta_tip} refs/libra/traces\0report-status object-format={kind}\n"
            ),
        );
        receive.extend_from_slice(b"0000");
        receive.extend_from_slice(&delta_pack);
        let rejected = raw_request(
            fixture.port,
            "POST",
            &format!("{locator}/git-receive-pack"),
            Endpoint::Receive.content_type(),
            &receive,
            &headers,
            false,
        )
        .await;
        assert_eq!(
            rejected.status, 400,
            "PackError::Delta retains the existing InvalidParameter mapping"
        );
        assert!(
            rejected
                .body
                .windows(b"invalid_parameter".len())
                .any(|bytes| bytes == b"invalid_parameter")
        );
        unchanged(&before, &snapshot(db).await);

        let pack = fixture.git(&["-C", "trace-source", "-c", pack_alias, "mtr11-pack"]);
        let mut receive = Vec::new();
        pkt(
            &mut receive,
            &format!(
                "{second} {delta_tip} refs/libra/traces\0report-status object-format={kind}\n"
            ),
        );
        receive.extend_from_slice(b"0000");
        receive.extend_from_slice(&pack);
        let mut socket = tokio::time::timeout(Duration::from_secs(10), async {
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", fixture.port)).await.expect("stalled receive connection");
            let head = format!("POST {locator}/git-receive-pack HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/x-git-receive-pack-request\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n", fixture.port, receive.len());
            socket.write_all(head.as_bytes()).await.expect("stalled receive headers");
            let mut interim = Vec::new();
            while !interim.ends_with(b"\r\n\r\n") {
                interim.push(socket.read_u8().await.expect("continue after authenticated body poll"));
                assert!(interim.len() < 64 * 1024, "bounded interim response");
            }
            assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n", "body polling follows authentication and capacity acquisition");
            socket
        }).await.expect("authenticated receive barrier deadline");
        let probe = raw_request(
            fixture.port,
            "POST",
            &format!("{locator}/git-receive-pack"),
            Endpoint::Receive.content_type(),
            b"0000",
            &headers,
            false,
        )
        .await;
        assert_eq!(
            probe.status, 503,
            "one probe after the stalled receive owns its permit"
        );
        let mut fetch = Vec::new();
        pkt(&mut fetch, &format!("want {first} object-format={kind}\n"));
        fetch.extend_from_slice(b"0000");
        pkt(&mut fetch, "done\n");
        let response = http_git_body(fixture, &by_scope, &token, false, &fetch).await;
        assert_eq!(
            response.status, 200,
            "fetch capacity is independent of a stalled receive"
        );
        assert!(http_git_unpack_response(&response.body, false).starts_with(b"PACK"));
        mega2_core::access_token::Entity::delete_by_id(token_id)
            .exec(db)
            .await
            .expect("revoke after authorization");
        let revoked_state = snapshot(db).await;
        let response = tokio::time::timeout(LIMIT, async {
            socket
                .write_all(&receive)
                .await
                .expect("supply previously authorized request body");
            let mut response = Vec::new();
            socket
                .take(64 * 1024)
                .read_to_end(&mut response)
                .await
                .expect("revoked receive response");
            assert!(response.len() < 64 * 1024);
            response
        })
        .await
        .expect("revoked receive deadline");
        assert!(
            response.starts_with(b"HTTP/1.1 404 "),
            "publication rechecks the live token witness"
        );
        assert!(
            response
                .windows(b"unknown_scope".len())
                .any(|part| part == b"unknown_scope")
        );
        assert!(
            !response
                .windows(b"unpack ok".len())
                .any(|part| part == b"unpack ok")
        );
        unchanged(&revoked_state, &snapshot(db).await);

        fixture.stop();
        let static_token = format!("mtr11-static-{}", Uuid::new_v4());
        let config_path = fixture.temp.path().join("config.toml");
        let config = fs::read_to_string(&config_path).expect("fixture config");
        fs::write(&config_path, format!("{config}\n[[git.push_tokens]]\nname = \"mtr11-static\"\ntoken = \"{static_token}\"\npaths = [\"/project\"]\n")).expect("static token fixture config");
        trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal),
            namespace: Set("static_push_token".to_owned()),
            credential_id: Set("mtr11-static".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .expect("bind configured static credential to same principal");
        fixture.static_auth = true;
        fixture.start(true, "trunk", "/third-party").await;
        let remote = git_cli::mega2_http_url(fixture.port, &by_scope);
        let push = http_git_authenticated(
            fixture,
            &static_token,
            &[
                "-C",
                "trace-source",
                "push",
                &remote,
                "HEAD:refs/libra/traces",
            ],
        );
        assert!(
            push.status.success(),
            "static credential by-scope update: {}",
            String::from_utf8_lossy(&push.stderr)
        );
        assert_eq!(
            trace_stream::Entity::find_by_id(&stream.id)
                .one(db)
                .await
                .expect("static push stream")
                .expect("scope")
                .tip_oid
                .as_deref(),
            Some(delta_tip.as_str())
        );
        let refs = http_git_authenticated(fixture, &static_token, &["ls-remote", &remote]);
        assert!(refs.status.success());
        assert_eq!(
            String::from_utf8(refs.stdout).expect("static refs"),
            format!("{delta_tip}\trefs/libra/traces\n")
        );
    }

    type BootstrapIndex = (BTreeSet<(String, String)>, BTreeMap<String, String>);

    fn bootstrap_git_index(fixture: &Fixture) -> BootstrapIndex {
        let listing = fixture.git_text(&[
            "-C",
            "bootstrap-index-oracle",
            "ls-tree",
            "-r",
            "-t",
            "HEAD",
        ]);
        let mut children: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
        for line in listing.lines() {
            let (header, path) = line.split_once('\t').expect("Git tree record");
            let fields: Vec<_> = header.split_whitespace().collect();
            assert_eq!(fields.len(), 3);
            let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            children.entry(parent.to_owned()).or_default().push((
                fields[1].to_owned(),
                fields[2].to_owned(),
                path.to_owned(),
            ));
        }
        let mut stack = vec![String::new()];
        let mut paths = BTreeSet::new();
        let mut legacy = BTreeMap::new();
        while let Some(parent) = stack.pop() {
            for (kind, oid, path) in children.get(&parent).into_iter().flatten() {
                if kind == "tree" {
                    stack.push(path.clone());
                } else {
                    assert_eq!(kind, "blob");
                    let absolute = format!("/{path}");
                    paths.insert((oid.clone(), absolute.clone()));
                    legacy.insert(oid.clone(), absolute);
                }
            }
        }
        assert!(!paths.is_empty(), "Git oracle must contain real root blobs");
        (paths, legacy)
    }

    async fn bootstrap_index_matches(
        db: &sea_orm::DatabaseConnection,
        expected: &BootstrapIndex,
    ) -> bool {
        let paths: BTreeSet<(String, String)> = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT blob_id, path FROM blob_paths",
            ))
            .await
            .expect("controlled index rows")
            .into_iter()
            .map(|row| {
                (
                    row.try_get("", "blob_id")
                        .expect("controlled blob identity"),
                    row.try_get("", "path").expect("controlled blob path"),
                )
            })
            .collect();
        let legacy: BTreeMap<String, String> = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT blob_id, file_path FROM mega_blob",
            ))
            .await
            .expect("controlled legacy rows")
            .into_iter()
            .map(|row| {
                (
                    row.try_get("", "blob_id")
                        .expect("controlled legacy identity"),
                    row.try_get("", "file_path")
                        .expect("controlled legacy path"),
                )
            })
            .collect();
        paths == expected.0
            && expected
                .1
                .iter()
                .all(|(oid, path)| legacy.get(oid) == Some(path))
    }

    pub async fn bootstrap_index_readiness(case: &str) {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = Fixture::new(&db).await;
        let mut lock = None;
        let result = AssertUnwindSafe(async {
            if case == "shared" {
                fixture.prepare(&db).await;
            } else {
                let mut init = fixture.command(false, "trunk", "/third-party");
                init.args(["service", "init", "--yes"]);
                OwnedChild::spawn(&mut init).require_success().await;
                fixture.start(true, "trunk", "/third-party").await;
            }
            fixture.git(&[
                "clone",
                &git_cli::mega2_http_url(fixture.port, "/"),
                "bootstrap-index-oracle",
            ]);
            let expected = bootstrap_git_index(&fixture);
            let setup_deadline = Instant::now() + LIMIT;
            while !bootstrap_index_matches(&db, &expected).await {
                assert!(Instant::now() < setup_deadline, "initial oracle setup deadline");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            fixture.stop();
            let (table, mutation, writer_pattern) = match case {
                "empty" => (
                    "blob_paths",
                    "DELETE FROM blob_paths",
                    "%INSERT INTO blob_paths%",
                ),
                "wrong" => (
                    "blob_paths",
                    "UPDATE blob_paths SET path='/fix23-wrong'||path",
                    "%INSERT INTO blob_paths%",
                ),
                "shared" => {
                    let mut appearances = BTreeMap::new();
                    for (oid, _) in &expected.0 {
                        *appearances.entry(oid).or_insert(0) += 1;
                    }
                    assert!(
                        appearances.values().any(|count| *count > 1),
                        "shared-blob case must contain actual multiple paths"
                    );
                    (
                        "mega_blob",
                        "UPDATE mega_blob SET file_path='/fix23-stale/'||blob_id",
                        "%UPDATE%mega_blob%",
                    )
                }
                _ => panic!("unknown bootstrap index case"),
            };
            db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, mutation))
                .await
                .expect("seed controlled incomplete index");
            let count: usize = scalar(&db, "SELECT count(*)::text AS value FROM blob_paths")
                .await
                .parse()
                .expect("controlled index count");
            assert_eq!(count, if case == "empty" { 0 } else { expected.0.len() });
            assert!(!bootstrap_index_matches(&db, &expected).await);

            lock = Some(db.begin().await.expect("index blocker transaction"));
            let txn = lock.as_ref().expect("held index blocker");
            let lock_timeout: String = txn
                .query_one_raw(Statement::from_string(DatabaseBackend::Postgres, "SHOW lock_timeout"))
                .await
                .expect("read lock timeout")
                .expect("lock timeout row")
                .try_get("", "lock_timeout")
                .expect("lock timeout setting");
            assert_eq!(lock_timeout, "0");
            let locker: i32 = txn
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT pg_backend_pid() AS pid",
                ))
                .await
                .expect("blocker backend")
                .expect("blocker row")
                .try_get("", "pid")
                .expect("blocker pid");
            txn.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("LOCK TABLE {table} IN SHARE MODE"),
            ))
            .await
            .expect("hold read-compatible index blocker");
            let log = fixture.temp.path().join("process.out");
            let offset = fs::read(&log).expect("prior service log").len();
            let premature = tokio::time::timeout(LIMIT, async {
                let mut start = Box::pin(fixture.start(true, "trunk", "/third-party"));
                let mut witness = Box::pin(async {
                    let client = reqwest::Client::builder()
                        .timeout(Duration::from_secs(2))
                        .build()
                        .expect("controlled listener client");
                    loop {
                        let blocked: bool = db
                            .query_one_raw(Statement::from_sql_and_values(
                                DatabaseBackend::Postgres,
                                "SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE a.datname=current_database() AND l.relation=$1::regclass AND NOT l.granted AND l.mode='RowExclusiveLock' AND $2=ANY(pg_blocking_pids(a.pid)) AND a.query LIKE $3) AS blocked",
                                [table.into(), locker.into(), writer_pattern.into()],
                            ))
                            .await
                            .expect("observe actual blocked index writer")
                            .expect("blocked writer row")
                            .try_get("", "blocked")
                            .expect("blocked writer boolean");
                        let bytes = fs::read(&log).expect("controlled service log");
                        let lines = String::from_utf8_lossy(&bytes[offset..]);
                        let port = lines.lines().find_map(|line| {
                            if !line.contains("HTTP server started up") {
                                return None;
                            }
                            line.split_once("address=")?
                                .1
                                .split_whitespace()
                                .next()?
                                .rsplit_once(':')?
                                .1
                                .parse::<u16>()
                                .ok()
                        });
                        if blocked
                            && let Some(port) = port
                            && let Ok(response) = client
                                .get(git_cli::mega2_host_http_url(port, "/api/openapi.json"))
                                .send()
                                .await
                            && response.status() == reqwest::StatusCode::OK
                        {
                            eprintln!("FIX23_WITNESS case={case} listener=200 blocked_writer=true lock_timeout=0");
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                });
                let mut premature = tokio::select! {
                    () = &mut start => {
                        witness.await;
                        true
                    }
                    () = &mut witness => false,
                };
                if !premature {
                    premature = tokio::time::timeout(Duration::from_secs(2), &mut start)
                        .await
                        .is_ok();
                }
                lock.take()
                    .expect("release exact blocker")
                    .rollback()
                    .await
                    .expect("release index blocker");
                if !premature {
                    start.await;
                }
                while !bootstrap_index_matches(&db, &expected).await {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                eprintln!("FIX23_COMPLETION case={case} real_index=true premature={premature}");
                premature
            })
            .await
            .expect("controlled startup and real index completion exceeded 90s");
            assert!(!premature, "fixture returned before initial index readiness");
        })
        .catch_unwind()
        .await;
        let rolled_back = if let Some(txn) = lock.take() {
            txn.rollback().await
        } else {
            Ok(())
        };
        let stopped = std::panic::catch_unwind(AssertUnwindSafe(|| fixture.stop()));
        let closed = db.close().await;
        drop(guard);
        rolled_back.expect("release blocker on failed readiness");
        if let Err(panic) = stopped {
            std::panic::resume_unwind(panic);
        }
        closed.expect("close controlled readiness database pool");
        if let Err(panic) = result {
            let logs = fixture.temp.keep();
            eprintln!("FIX23 retained fixture: {}", logs.display());
            std::panic::resume_unwind(panic);
        }
    }

    pub async fn http_git_roundtrip(kind: &str) {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = Fixture::new(&db).await;
        let result = AssertUnwindSafe(http_git_roundtrip_inner(&mut fixture, &db, kind))
            .catch_unwind()
            .await;
        fixture.stop();
        let closed = db.close().await;
        drop(guard);
        if let Err(panic) = result {
            let logs = fixture.temp.keep();
            eprintln!("HTTP traces fixture logs retained at {}", logs.display());
            std::panic::resume_unwind(panic);
        }
        closed.expect("close HTTP traces pool");
    }

    pub async fn code_ref_boundary_legacy_advertisement() {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = Fixture::new(&db).await;
        let result = AssertUnwindSafe(async {
            fixture.prepare(&db).await;
            let import_url =
                git_cli::mega2_http_url(fixture.port, "/third-party/mtr94-control.git");
            let code_tip = fixture.git_text(&["-C", "import", "rev-parse", "HEAD"]);
            let code_shared_blob = fixture.git_text(&[
                "-C",
                "import",
                "rev-parse",
                &format!("{code_tip}:sentinel.txt"),
            ]);
            fixture.git(&["-C", "import", "switch", "--orphan", "trace-records"]);
            fs::write(fixture.shared.path().join("import/sentinel.txt"), SENTINEL)
                .expect("shared code and traces blob");
            fs::write(
                fixture.shared.path().join("import/trace-only.txt"),
                b"legacy trace-only sentinel\n",
            )
            .expect("trace-only source file");
            fixture.git(&["-C", "import", "add", "-A"]);
            fixture.git(&[
                "-C",
                "import",
                "-c",
                "user.name=Traces Test",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "legacy traces orphan history",
            ]);
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &import_url,
                "HEAD:refs/libra/traces",
            ]);
            let traces_tip = fixture.git_text(&["-C", "import", "rev-parse", "HEAD"]);
            let traces_tree =
                fixture.git_text(&["-C", "import", "rev-parse", "HEAD^{tree}"]);
            let traces_blob =
                fixture.git_text(&["-C", "import", "rev-parse", "HEAD:trace-only.txt"]);
            let traces_shared_blob =
                fixture.git_text(&["-C", "import", "rev-parse", "HEAD:sentinel.txt"]);
            assert_eq!(
                traces_shared_blob, code_shared_blob,
                "fixture must really share a blob across code and legacy traces history"
            );
            for version in ["0", "2"] {
                let refs = fixture.git_text(&[
                    "-c",
                    &format!("protocol.version={version}"),
                    "ls-remote",
                    &import_url,
                ]);
                assert!(
                    !refs.contains("refs/libra/traces"),
                    "ordinary code advertisement exposed legacy traces in v{version}: {refs}"
                );
                assert!(
                    refs.lines()
                        .any(|line| line == format!("{code_tip}\tHEAD")),
                    "ordinary code HEAD did not remain at its authorized tip in v{version}: {refs}"
                );
            }
            let traces_only_url =
                git_cli::mega2_http_url(fixture.port, "/third-party/mtr40-traces-only.git");
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &traces_only_url,
                "HEAD:refs/libra/traces",
            ]);
            for version in ["0", "2"] {
                let refs = fixture.git_text(&[
                    "-c",
                    &format!("protocol.version={version}"),
                    "ls-remote",
                    &traces_only_url,
                ]);
                assert!(
                    refs.is_empty(),
                    "traces-only legacy repository must have no code HEAD or advertised code refs in v{version}: {refs}"
                );
            }
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &traces_only_url,
                &format!("{code_tip}:refs/heads/main"),
            ]);
            for version in ["0", "2"] {
                let refs = fixture.git_text(&[
                    "-c",
                    &format!("protocol.version={version}"),
                    "ls-remote",
                    &traces_only_url,
                ]);
                assert!(
                    refs.lines()
                        .any(|line| line == format!("{code_tip}\tHEAD")),
                    "code HEAD must recover after a legacy traces ref was the first and default ref in v{version}: {refs}"
                );
            }
            for (kind, oid) in [
                ("commit", &traces_tip),
                ("tree", &traces_tree),
                ("blob", &traces_blob),
            ] {
                for v2 in [false, true] {
                    let mut body = Vec::new();
                    if v2 {
                        pkt(&mut body, "command=fetch\n");
                        body.extend_from_slice(b"0001");
                        pkt(&mut body, &format!("want {oid}\n"));
                        pkt(&mut body, "done\n");
                        body.extend_from_slice(b"0000");
                    } else {
                        pkt(&mut body, &format!("want {oid}\n"));
                        body.extend_from_slice(b"0000");
                        pkt(&mut body, "done\n");
                    }
                    let response = request(
                        fixture.port,
                        Endpoint::Upload,
                        "/third-party/mtr94-control.git",
                        v2,
                        &body,
                        None,
                        false,
                    )
                    .await;
                    assert_eq!(
                        response.status, 404,
                        "traces-only {kind} want must be rejected in v{v2}"
                    );
                    assert!(
                        response
                            .body
                            .windows(b"unknown_scope".len())
                            .any(|window| window == b"unknown_scope"),
                        "traces-only {kind} want must report the stable unknown_scope code"
                    );
                    assert!(
                        !response.body.windows(4).any(|window| window == b"PACK"),
                        "traces-only {kind} want must not produce a code pack"
                    );
                }
            }
            for v2 in [false, true] {
                let mut body = Vec::new();
                if v2 {
                    pkt(&mut body, "command=fetch\n");
                    body.extend_from_slice(b"0001");
                    pkt(&mut body, &format!("want {code_tip}\n"));
                    pkt(&mut body, &format!("have {traces_tip}\n"));
                    body.extend_from_slice(b"0000");
                } else {
                    pkt(&mut body, &format!("want {code_tip} multi_ack_detailed\n"));
                    body.extend_from_slice(b"0000");
                    pkt(&mut body, &format!("have {traces_tip}\n"));
                    pkt(&mut body, "done\n");
                }
                let response = request(
                    fixture.port,
                    Endpoint::Upload,
                    "/third-party/mtr94-control.git",
                    v2,
                    &body,
                    None,
                    false,
                )
                .await;
                assert_eq!(response.status, 200, "a trace-only local have must not break code fetch in v{v2}");
                assert_eq!(
                    response.body.windows(4).any(|window| window == b"PACK"),
                    !v2,
                    "v0 done fetch gets a full pack; v2 no-done round gets only NAK"
                );
                assert!(
                    !response.body.windows(format!("ACK {traces_tip}").len()).any(|window| window == format!("ACK {traces_tip}").as_bytes()),
                    "trace-only have must be dropped before ACK and pack generation"
                );
            }
            let unknown_have = "f".repeat(40);
            for v2 in [false, true] {
                let mut body = Vec::new();
                if v2 {
                    pkt(&mut body, "command=fetch\n");
                    body.extend_from_slice(b"0001");
                    pkt(&mut body, &format!("want {code_tip}\n"));
                    pkt(&mut body, &format!("have {unknown_have}\n"));
                    pkt(&mut body, "done\n");
                    body.extend_from_slice(b"0000");
                } else {
                    pkt(&mut body, &format!("want {code_tip} multi_ack_detailed\n"));
                    body.extend_from_slice(b"0000");
                    pkt(&mut body, &format!("have {unknown_have}\n"));
                    pkt(&mut body, "done\n");
                }
                let response = request(
                    fixture.port,
                    Endpoint::Upload,
                    "/third-party/mtr94-control.git",
                    v2,
                    &body,
                    None,
                    false,
                )
                .await;
                assert_eq!(
                    response.status, 200,
                    "an unknown local have must not break an ordinary code fetch in v{v2}"
                );
                assert!(
                    response.body.windows(4).any(|window| window == b"PACK"),
                    "an unknown local have still permits a code pack"
                );
            }
            for v2 in [false, true] {
                let mut body = Vec::new();
                if v2 {
                    pkt(&mut body, "command=fetch\n");
                    body.extend_from_slice(b"0001");
                    pkt(&mut body, &format!("want {code_tip}\n"));
                    pkt(&mut body, &format!("have {code_tip}\n"));
                    body.extend_from_slice(b"0000");
                } else {
                    pkt(&mut body, &format!("want {code_tip} multi_ack_detailed\n"));
                    body.extend_from_slice(b"0000");
                    pkt(&mut body, &format!("have {code_tip}\n"));
                    pkt(&mut body, "done\n");
                }
                let response = request(
                    fixture.port,
                    Endpoint::Upload,
                    "/third-party/mtr94-control.git",
                    v2,
                    &body,
                    None,
                    false,
                )
                .await;
                let expected_ack = format!("ACK {code_tip}");
                assert_eq!(response.status, 200, "authorized code have remains usable in v{v2}");
                assert!(response.body.windows(expected_ack.len()).any(|window| window == expected_ack.as_bytes()));
                assert!(response.body.windows(4).any(|window| window == b"PACK"));
            }
            for v2 in [false, true] {
                let mut body = Vec::new();
                if v2 {
                    pkt(&mut body, "command=fetch\n");
                    body.extend_from_slice(b"0001");
                    pkt(&mut body, &format!("want {code_shared_blob}\n"));
                    pkt(&mut body, "done\n");
                    body.extend_from_slice(b"0000");
                } else {
                    pkt(&mut body, &format!("want {code_shared_blob}\n"));
                    body.extend_from_slice(b"0000");
                    pkt(&mut body, "done\n");
                }
                let response = request(
                    fixture.port,
                    Endpoint::Upload,
                    "/third-party/mtr94-control.git",
                    v2,
                    &body,
                    None,
                    false,
                )
                .await;
                assert_eq!(
                    response.status, 200,
                    "a shared blob reachable from a code ref remains fetchable in v{v2}"
                );
                assert!(response.body.windows(4).any(|window| window == b"PACK"));
            }
            fixture.git(&["clone", &import_url, "mtr40-code-check"]);
            assert_eq!(
                fs::read(fixture.shared.path().join("mtr40-code-check/sentinel.txt"))
                    .expect("authorized code blob after trace history"),
                SENTINEL,
                "a blob shared by code and traces remains readable in the code view"
            );
            fixture.git(&["-C", "import", "switch", "-c", "mixed-code"]);
            fs::write(
                fixture.shared.path().join("import/mixed-code.txt"),
                b"code commit descended from legacy traces\n",
            )
            .expect("mixed code source file");
            fixture.git(&["-C", "import", "add", "mixed-code.txt"]);
            fixture.git(&[
                "-C",
                "import",
                "-c",
                "user.name=Traces Test",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "code history mixed with legacy traces",
            ]);
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &import_url,
                "HEAD:refs/heads/mixed-code",
            ]);
            let mixed_tip = fixture.git_text(&["-C", "import", "rev-parse", "HEAD"]);
            for v2 in [false, true] {
                let mut command = Vec::new();
                if v2 {
                    pkt(&mut command, "command=ls-refs\n");
                    command.extend_from_slice(b"0001");
                    pkt(&mut command, "symrefs\n");
                    command.extend_from_slice(b"0000");
                }
                let response = request(
                    fixture.port,
                    if v2 {
                        Endpoint::Upload
                    } else {
                        Endpoint::Discovery
                    },
                    "/third-party/mtr94-control.git",
                    v2,
                    &command,
                    None,
                    false,
                )
                .await;
                assert_eq!(response.status, 200, "clean code refs remain visible in v{v2}");
                assert!(
                    response
                        .body
                        .windows(code_tip.len())
                        .any(|window| window == code_tip.as_bytes()),
                    "clean main must remain discoverable"
                );
                assert!(
                    !response.body.windows(b"refs/heads/mixed-code".len()).any(|window| window == b"refs/heads/mixed-code"),
                    "contaminated ref must be isolated from upload discovery"
                );
                let mut want = Vec::new();
                if v2 {
                    pkt(&mut want, "command=fetch\n");
                    want.extend_from_slice(b"0001");
                    pkt(&mut want, &format!("want {mixed_tip}\n"));
                    pkt(&mut want, "done\n");
                    want.extend_from_slice(b"0000");
                } else {
                    pkt(&mut want, &format!("want {mixed_tip}\n"));
                    want.extend_from_slice(b"0000");
                    pkt(&mut want, "done\n");
                }
                let rejected = request(
                    fixture.port,
                    Endpoint::Upload,
                    "/third-party/mtr94-control.git",
                    v2,
                    &want,
                    None,
                    false,
                )
                .await;
                assert_eq!(rejected.status, 409, "mixed tip want must be rejected");
                assert!(rejected.body.windows(b"trace_history_in_code".len()).any(|window| window == b"trace_history_in_code"));
                assert!(!rejected.body.windows(4).any(|window| window == b"PACK"));

                let mut trace_want = Vec::new();
                if v2 {
                    pkt(&mut trace_want, "command=fetch\n");
                    trace_want.extend_from_slice(b"0001");
                    pkt(&mut trace_want, &format!("want {traces_tip}\n"));
                    pkt(&mut trace_want, "done\n");
                    trace_want.extend_from_slice(b"0000");
                } else {
                    pkt(&mut trace_want, &format!("want {traces_tip}\n"));
                    trace_want.extend_from_slice(b"0000");
                    pkt(&mut trace_want, "done\n");
                }
                let trace_rejected = request(
                    fixture.port,
                    Endpoint::Upload,
                    "/third-party/mtr94-control.git",
                    v2,
                    &trace_want,
                    None,
                    false,
                )
                .await;
                assert_eq!(trace_rejected.status, 404, "legacy trace tip remains outside the code view after contamination");
                assert!(trace_rejected.body.windows(b"unknown_scope".len()).any(|window| window == b"unknown_scope"));
            }
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &import_url,
                ":refs/heads/mixed-code",
            ]);
            for version in ["0", "2"] {
                let refs = fixture.git_text(&[
                    "-c",
                    &format!("protocol.version={version}"),
                    "ls-remote",
                    &import_url,
                ]);
                assert!(
                    refs.lines()
                        .any(|line| line == format!("{code_tip}\tHEAD")),
                    "explicit contaminated-ref deletion must restore the clean code view in v{version}: {refs}"
                );
                assert!(!refs.contains("refs/heads/mixed-code"), "repair must actually remove the polluted ref");
                let v2 = version == "2";
                let mut stale_have = Vec::new();
                if v2 {
                    pkt(&mut stale_have, "command=fetch\n");
                    stale_have.extend_from_slice(b"0001");
                    pkt(&mut stale_have, &format!("want {code_tip}\n"));
                    pkt(&mut stale_have, &format!("have {mixed_tip}\n"));
                    pkt(&mut stale_have, "done\n");
                    stale_have.extend_from_slice(b"0000");
                } else {
                    pkt(&mut stale_have, &format!("want {code_tip} multi_ack_detailed\n"));
                    stale_have.extend_from_slice(b"0000");
                    pkt(&mut stale_have, &format!("have {mixed_tip}\n"));
                    pkt(&mut stale_have, "done\n");
                }
                let response = request(
                    fixture.port,
                    Endpoint::Upload,
                    "/third-party/mtr94-control.git",
                    v2,
                    &stale_have,
                    None,
                    false,
                )
                .await;
                assert_eq!(response.status, 200, "stale local have must not break fetch after repair");
                assert!(response.body.windows(4).any(|window| window == b"PACK"));
                let stale_ack = format!("ACK {mixed_tip}");
                assert!(!response.body.windows(stale_ack.len()).any(|window| window == stale_ack.as_bytes()));

                let mut stale_want = Vec::new();
                if v2 {
                    pkt(&mut stale_want, "command=fetch\n");
                    stale_want.extend_from_slice(b"0001");
                    pkt(&mut stale_want, &format!("want {mixed_tip}\n"));
                    pkt(&mut stale_want, "done\n");
                    stale_want.extend_from_slice(b"0000");
                } else {
                    pkt(&mut stale_want, &format!("want {mixed_tip}\n"));
                    stale_want.extend_from_slice(b"0000");
                    pkt(&mut stale_want, "done\n");
                }
                let response = request(
                    fixture.port,
                    Endpoint::Upload,
                    "/third-party/mtr94-control.git",
                    v2,
                    &stale_want,
                    None,
                    false,
                )
                .await;
                assert_eq!(response.status, 404, "deleted polluted tip is no longer code-reachable");
                assert!(response.body.windows(b"unknown_scope".len()).any(|window| window == b"unknown_scope"));
                assert!(!response.body.windows(4).any(|window| window == b"PACK"));
            }
            assert_eq!(fixture.git_text(&["-C", "import", "rev-parse", "refs/heads/trace-records"]), traces_tip);
            assert_eq!(fixture.git_text(&["-C", "import", "rev-parse", "refs/heads/mixed-code"]), mixed_tip);
            fixture.git(&["clone", &import_url, "mtr40-code-writer"]);
            for version in ["0", "2"] {
                let filename = format!("fetch-v{version}.txt");
                fs::write(fixture.shared.path().join("mtr40-code-writer").join(&filename), version)
                    .expect("new remote code commit");
                fixture.git(&["-C", "mtr40-code-writer", "add", &filename]);
                fixture.git(&[
                    "-C", "mtr40-code-writer", "-c", "user.name=Traces Test",
                    "-c", "user.email=traces@example.invalid", "-c", "commit.gpgsign=false",
                    "commit", "-m", &format!("code fetch v{version} positive control"),
                ]);
                fixture.git(&[
                    "-C", "mtr40-code-writer", "push", "--no-thin", &import_url,
                    "HEAD:refs/heads/main",
                ]);
                let expected = fixture.git_text(&["-C", "mtr40-code-writer", "rev-parse", "HEAD"]);
                fixture.git(&[
                    "-C", "import", "-c", &format!("protocol.version={version}"),
                    "fetch", &import_url, "refs/heads/main",
                ]);
                assert_eq!(
                    fixture.git_text(&["-C", "import", "rev-parse", "FETCH_HEAD"]),
                    expected,
                    "real Git fetch must succeed with local traces and stale polluted refs in v{version}"
                );
            }
            let polluted_only_url =
                git_cli::mega2_http_url(fixture.port, "/third-party/mtr40-polluted-only.git");
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &polluted_only_url,
                &format!("{mixed_tip}:refs/heads/mixed-code"),
            ]);
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &polluted_only_url,
                &format!("{traces_tip}:refs/libra/traces"),
            ]);
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &polluted_only_url,
                ":refs/heads/mixed-code",
            ]);
            for version in ["0", "2"] {
                let refs = fixture.git_text(&[
                    "-c",
                    &format!("protocol.version={version}"),
                    "ls-remote",
                    &polluted_only_url,
                ]);
                assert!(refs.is_empty(), "delete-only repair must leave an unborn code HEAD in v{version}: {refs}");
            }
            let dev_only_url =
                git_cli::mega2_http_url(fixture.port, "/third-party/mtr40-dev-only.git");
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &dev_only_url,
                &format!("{traces_tip}:refs/libra/traces"),
            ]);
            fixture.git(&[
                "-C",
                "import",
                "push",
                "--no-thin",
                &dev_only_url,
                &format!("{code_tip}:refs/heads/dev"),
            ]);
            let symrefs = fixture.git_text(&[
                "-c",
                "protocol.version=2",
                "ls-remote",
                "--symref",
                &dev_only_url,
            ]);
            assert!(symrefs.lines().any(|line| line == "ref: refs/heads/dev\tHEAD"), "v2 HEAD symref must name the visible fallback branch: {symrefs}");
        })
        .catch_unwind()
        .await;
        fixture.stop();
        let closed = db.close().await;
        drop(guard);
        if let Err(panic) = result {
            for name in ["process.out", "process.err"] {
                if let Ok(bytes) = fs::read(fixture.temp.path().join(name)) {
                    eprintln!(
                        "legacy fixture failure log: file={name} sha256={}",
                        hex::encode(Sha256::digest(&bytes))
                    );
                }
            }
            std::panic::resume_unwind(panic);
        }
        closed.expect("close legacy ref fixture pool before database guard");
    }

    pub async fn run(endpoint: Endpoint) {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(Fixture::new(&db).await);
            let fixture = fixture.as_mut().expect("fixture");
            fixture.prepare(&db).await;
            if matches!(endpoint, Endpoint::Discovery) {
                fixture.lfs_controls(&db, false).await;
                fixture.disabled_lfs(&db).await;
            }
            fixture.disabled(&db, endpoint).await;
            if matches!(endpoint, Endpoint::Discovery) {
                fixture.disabled(&db, Endpoint::ReceiveDiscovery).await;
            }
            let disabled_url = git_cli::mega2_http_url(
                fixture.port,
                &format!("/.traces/by-scope/{}.git", fixture.scope),
            );
            let before = snapshot(&db).await;
            let output = git_cli::git_cli_no_auth(
                fixture.shared.path(),
                &["clone", &disabled_url, "must-not-clone"],
            );
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("404")
                    || String::from_utf8_lossy(&output.stderr).contains("not found")
            );
            unchanged(&before, &snapshot(&db).await);
            let output = git_cli::git_cli_no_auth(
                fixture.shared.path(),
                &[
                    "-C",
                    "import",
                    "push",
                    &disabled_url,
                    "HEAD:refs/libra/traces",
                ],
            );
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("404")
                    || String::from_utf8_lossy(&output.stderr).contains("not found")
            );
            unchanged(&before, &snapshot(&db).await);
            fixture.relocate_import(&db).await;
            fixture.disabled(&db, endpoint).await;
            if matches!(endpoint, Endpoint::Discovery) {
                fixture.disabled_lfs(&db).await;
                fixture.disabled(&db, Endpoint::ReceiveDiscovery).await;
            }
            fixture.enabled(&db, endpoint).await;
            if matches!(endpoint, Endpoint::Discovery) {
                fixture.lfs_controls(&db, true).await;
                fixture.start(false, "review", "/%2etraces").await;
                fixture.converge(&db).await;
                fixture.disabled_lfs(&db).await;
                fixture.disabled(&db, endpoint).await;
                fixture.disabled(&db, Endpoint::ReceiveDiscovery).await;
            }
        })
        .catch_unwind()
        .await;
        let cleanup = std::panic::catch_unwind(AssertUnwindSafe(|| {
            if let Some(fixture) = &mut fixture {
                fixture.stop();
            }
        }));
        let closed = db.close().await;
        drop(guard);
        if (result.is_err() || cleanup.is_err())
            && let Some(fixture) = fixture.take()
        {
            let log_dir = fixture.temp.keep();
            for name in ["process.out", "process.err"] {
                if let Ok(bytes) = fs::read(log_dir.join(name)) {
                    let digest = Sha256::digest(&bytes);
                    eprintln!(
                        "HTTP fixture failure log retained: file={} sha256={}",
                        log_dir.join(name).display(),
                        hex::encode(digest)
                    );
                }
            }
        }
        drop(fixture);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        if let Err(panic) = cleanup {
            std::panic::resume_unwind(panic);
        }
        closed.expect("close HTTP fixture pool before database guard");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_ref_boundary_legacy_advertisement() {
    disabled_http::code_ref_boundary_legacy_advertisement().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_git_bootstrap_index_empty() {
    disabled_http::bootstrap_index_readiness("empty").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_git_bootstrap_index_wrong_paths() {
    disabled_http::bootstrap_index_readiness("wrong").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_git_bootstrap_index_shared_blob_file_paths() {
    disabled_http::bootstrap_index_readiness("shared").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_git_sha1_roundtrip() {
    disabled_http::http_git_roundtrip("sha1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_git_sha256_roundtrip() {
    disabled_http::http_git_roundtrip("sha256").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_disabled_http_discovery() {
    disabled_http::run(disabled_http::Endpoint::Discovery).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_disabled_http_upload() {
    disabled_http::run(disabled_http::Endpoint::Upload).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_disabled_http_receive() {
    disabled_http::run(disabled_http::Endpoint::Receive).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_disabled_ssh_upload() {
    disabled_http::run_ssh(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_disabled_ssh_receive() {
    disabled_http::run_ssh(true).await;
}
