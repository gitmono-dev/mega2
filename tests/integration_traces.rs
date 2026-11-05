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
        object_format: String,
    }

    fn fixture_file_sha256(path: &Path) -> String {
        let mut file = fs::File::open(path).expect("open pinned fixture input");
        let mut digest = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let size =
                std::io::Read::read(&mut file, &mut buffer).expect("hash pinned fixture input");
            if size == 0 {
                break;
            }
            digest.update(&buffer[..size]);
        }
        hex::encode(digest.finalize())
    }

    struct ProviderBridgeChild(Option<Child>);

    impl Drop for ProviderBridgeChild {
        fn drop(&mut self) {
            let Some(mut child) = self.0.take() else {
                return;
            };
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            #[cfg(unix)]
            {
                let group = i32::try_from(child.id()).expect("owned provider process group");
                // This unreaped child was created as its own group leader;
                // its PID cannot be reused while we signal its owned group.
                unsafe {
                    libc::kill(-group, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        return;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
            }
            child.wait().expect("reap owned provider bridge");
        }
    }

    async fn qualify_fixture_local_root(fixture: &Fixture) {
        // Normal repository tests remain self-contained. Formal MTR92/C
        // windows require and pin both environment inputs in their runner.
        let Some(manifest_path) = std::env::var_os("MEGA2_MTR92_PROVIDER_QUALIFICATION_MANIFEST")
        else {
            return;
        };
        let manifest_path = PathBuf::from(manifest_path);
        assert!(
            manifest_path.is_absolute(),
            "explicit provider manifest path"
        );
        let manifest_bytes = fs::read(&manifest_path).expect("provider qualification manifest");
        let manifest_sha = hex::encode(Sha256::digest(&manifest_bytes));
        let manifest: serde_json::Value =
            serde_json::from_slice(&manifest_bytes).expect("provider manifest JSON");
        assert_eq!(manifest["schema_version"].as_u64(), Some(1));
        assert_eq!(
            manifest["kind"].as_str(),
            Some("mega2-local-provider-qualification")
        );
        assert_eq!(
            manifest["local_requires_exact_fixture_probe"].as_bool(),
            Some(true)
        );
        let bridge = &manifest["bridge"];
        let text = |value: &serde_json::Value| {
            value.as_str().expect("provider manifest string").to_owned()
        };
        let script = PathBuf::from(text(&bridge["script_path"]));
        let python = PathBuf::from(text(&bridge["python_path"]));
        let artifact = PathBuf::from(text(&manifest["archived_libtest"]["path"]));
        assert!(script.is_absolute() && python.is_absolute() && artifact.is_absolute());
        assert_eq!(fixture_file_sha256(&script), text(&bridge["script_sha256"]));
        assert_eq!(fixture_file_sha256(&python), text(&bridge["python_sha256"]));
        assert_eq!(
            fixture_file_sha256(&artifact),
            text(&manifest["archived_libtest"]["sha256"])
        );
        let seconds = bridge["total_budget_seconds"]
            .as_u64()
            .expect("declared provider bridge budget");
        assert!(
            (1..=180).contains(&seconds),
            "provider bridge keeps its declared aggregate budget"
        );
        let archive = PathBuf::from(
            std::env::var_os("MEGA2_MTR92_PROVIDER_ROOT_RECEIPTS")
                .expect("provider manifest requires durable receipt archive"),
        );
        assert!(archive.is_absolute());
        let archive_metadata =
            fs::symlink_metadata(&archive).expect("existing durable provider archive");
        assert!(archive_metadata.is_dir() && !archive_metadata.file_type().is_symlink());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(archive_metadata.permissions().mode() & 0o777, 0o700);
        }
        let root = fixture.temp.path().join("objects");
        assert!(
            !root.exists(),
            "a new fixture owns its fresh exact object root"
        );
        let mut directory = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory
            .create(&root)
            .expect("create exact fixture Local root");
        let root = root.canonicalize().expect("canonical fixture Local root");
        let receipt_dir = archive
            .canonicalize()
            .expect("canonical durable provider archive")
            .join(format!("fixture-{}", Uuid::new_v4()));
        assert!(!receipt_dir.exists());
        let receipt_path = receipt_dir.join("ROOT-RECEIPT.json");
        let mut command = Command::new(&python);
        command
            .env_clear()
            .current_dir(fixture.temp.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .arg(&script)
            .arg("--manifest")
            .arg(&manifest_path)
            .arg("--root")
            .arg(&root)
            .arg("--receipt-dir")
            .arg(&receipt_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(fixture.temp.path().join("provider-bridge.stderr"))
                    .expect("private provider bridge stderr"),
            ));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = ProviderBridgeChild(Some(
            command
                .spawn()
                .expect("start pinned exact-root provider bridge"),
        ));
        // The bridge's 180s includes its internal 20s cleanup reserve. The
        // outer 5s allows its controlled stop receipt; Drop owns TERM/KILL.
        let deadline = Instant::now() + Duration::from_secs(seconds + 5);
        let status = loop {
            if let Some(status) = child
                .0
                .as_mut()
                .expect("owned provider bridge")
                .try_wait()
                .expect("poll provider bridge")
            {
                child.0.take();
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "provider bridge exceeded declared budget plus 5s closeout"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(
            status.success(),
            "exact-root provider qualification failed: {status}"
        );
        let receipt_bytes = fs::read(&receipt_path).expect("actual exact-root provider receipt");
        let receipt: serde_json::Value =
            serde_json::from_slice(&receipt_bytes).expect("exact-root receipt JSON");
        assert_eq!(receipt["accepted"].as_bool(), Some(true));
        assert_eq!(receipt["total_cases"].as_u64(), Some(16));
        assert_eq!(receipt["completed_cases"].as_u64(), Some(16));
        assert_eq!(
            receipt["cases"]
                .as_array()
                .expect("exact-root actual cases")
                .len(),
            16
        );
        assert_eq!(
            receipt["local_root"]["canonical_path"].as_str(),
            root.to_str()
        );
        assert_eq!(
            receipt["local_root_after"]["canonical_path"].as_str(),
            root.to_str()
        );
        for key in ["device", "inode", "uid", "gid", "mode"] {
            assert_eq!(receipt["local_root"][key], receipt["local_root_after"][key]);
        }
        assert_eq!(
            receipt["cleanup"]["all_owned_keys_absent"].as_bool(),
            Some(true)
        );
        assert_eq!(
            receipt["cleanup"]["all_children_reaped"].as_bool(),
            Some(true)
        );
        assert!(
            receipt["cleanup"]["errors"]
                .as_array()
                .expect("provider cleanup errors")
                .is_empty()
        );
        assert!(
            receipt["errors"]
                .as_array()
                .expect("provider errors")
                .is_empty()
        );
        assert_eq!(
            receipt["manifest_sha256"].as_str(),
            Some(manifest_sha.as_str())
        );
        assert_eq!(receipt["bridge_sha256"], bridge["script_sha256"]);
        assert_eq!(
            receipt["artifact_sha256"],
            manifest["archived_libtest"]["sha256"]
        );
        assert_eq!(
            receipt["provider_source_before_sha256"],
            receipt["provider_source_after_sha256"]
        );
        let summary = serde_json::json!({
            "schema_version": 1,
            "root_path": root.to_str().expect("UTF8 fixture root"),
            "root_sha256": hex::encode(Sha256::digest(root.to_str().expect("UTF8 fixture root").as_bytes())),
            "receipt_path": receipt_path,
            "receipt_sha256": hex::encode(Sha256::digest(&receipt_bytes)),
            "manifest_sha256": manifest_sha,
            "cases": 16,
        });
        // Direct stderr avoids splitting libtest's partial stdout identity.
        let mut stderr = std::io::stderr().lock();
        std::io::Write::write_all(
            &mut stderr,
            format!("MTR92_PROVIDER_ROOT_OK {summary}\n").as_bytes(),
        )
        .expect("write provider root receipt marker");
        std::io::Write::flush(&mut stderr).expect("flush provider root receipt marker");
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
                    .expect("make the owned fixture directory private");
            }
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
            let fixture = Self {
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
                object_format: std::env::var("MEGA_TRACES_TEST_FORMAT")
                    .unwrap_or_else(|_| "sha1".to_owned()),
            };
            qualify_fixture_local_root(&fixture).await;
            fixture
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
                .env("MEGA_MONOREPO__OBJECT_FORMAT", &self.object_format)
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
                &format!("--object-format={}", self.object_format),
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
        /// Issue a v0 upload-pack request whose want line carries `include-tag` and
        /// which asks for no tag ref, then return the deframed pack bytes. Stock git
        /// cannot isolate this: `--no-tags` suppresses the capability and
        /// `--tags`/clone fetch the tag ref anyway.
        async fn crafted_include_tag_pack(&self, path: &str, want_oid: &str) -> Vec<u8> {
            let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4);
            let body = format!(
                "{}{}{}",
                pkt(&format!("want {want_oid} include-tag side-band-64k\n")),
                "0000",
                pkt("done\n")
            );
            let response = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    self.port,
                    &format!("{path}/git-upload-pack"),
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body(body)
                .send()
                .await
                .expect("crafted upload-pack request");
            let status = response.status();
            let bytes = response.bytes().await.expect("crafted response body");
            let mut pack: Vec<u8> = Vec::new();
            let mut offset = 0usize;
            while offset + 4 <= bytes.len() {
                let header = String::from_utf8_lossy(&bytes[offset..offset + 4]).to_string();
                offset += 4;
                if header == "0000" {
                    continue;
                }
                let Ok(len) = usize::from_str_radix(&header, 16) else {
                    break;
                };
                if len == 0 || offset + len - 4 > bytes.len() {
                    break;
                }
                let payload = &bytes[offset..offset + len - 4];
                offset += len - 4;
                if payload.first() == Some(&1) {
                    pack.extend_from_slice(&payload[1..]);
                }
            }
            assert!(
                pack.starts_with(b"PACK"),
                "crafted include-tag request returned no pack (status {status})"
            );
            pack
        }

        /// Same crafted request over protocol v2: `command=fetch` with
        /// `include-tag` in the argument list, then the `packfile` section is
        /// walked for side-band data.
        async fn crafted_include_tag_pack_v2(&self, path: &str, want_oid: &str) -> Vec<u8> {
            let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4);
            let body = format!(
                "{}{}0001{}{}{}0000",
                pkt("command=fetch\n"),
                pkt("agent=git/2\n"),
                pkt(&format!("want {want_oid}\n")),
                pkt("include-tag\n"),
                pkt("done\n")
            );
            let response = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    self.port,
                    &format!("{path}/git-upload-pack"),
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .header("git-protocol", "version=2")
                .body(body)
                .send()
                .await
                .expect("crafted v2 request");
            let status = response.status();
            let bytes = response.bytes().await.expect("crafted v2 body");
            let start = bytes
                .windows(8)
                .position(|window| window == b"packfile")
                .unwrap_or_else(|| {
                    panic!("v2 include-tag response had no packfile section (status {status})")
                });
            let mut pack: Vec<u8> = Vec::new();
            let mut offset = start - 4; // point at the pkt-line length
            while offset + 4 <= bytes.len() {
                let header = String::from_utf8_lossy(&bytes[offset..offset + 4]).to_string();
                offset += 4;
                if header == "0000" {
                    break;
                }
                let Ok(len) = usize::from_str_radix(&header, 16) else {
                    break;
                };
                // Delimiters are length 1 and carry no payload.
                if len < 4 || offset + len - 4 > bytes.len() {
                    continue;
                }
                let payload = &bytes[offset..offset + len - 4];
                offset += len - 4;
                if payload.first() == Some(&1) {
                    pack.extend_from_slice(&payload[1..]);
                }
            }
            assert!(
                pack.starts_with(b"PACK"),
                "crafted v2 include-tag request returned no pack (status {status})"
            );
            pack
        }

        /// Crafted v0 shallow fetch that also asks for include-tag: the want line
        /// carries the capability and a `deepen` line limits the walk.
        async fn crafted_shallow_include_tag_pack(
            &self,
            path: &str,
            want_oid: &str,
            depth: u32,
        ) -> Vec<u8> {
            let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4);
            let body = format!(
                "{}{}{}{}{}",
                pkt(&format!("want {want_oid} include-tag side-band-64k\n")),
                pkt(&format!("deepen {depth}\n")),
                "0000",
                pkt("done\n"),
                ""
            );
            let response = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    self.port,
                    &format!("{path}/git-upload-pack"),
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body(body)
                .send()
                .await
                .expect("crafted shallow request");
            let status = response.status();
            let bytes = response.bytes().await.expect("crafted shallow body");
            // The response is side-band framed, so slicing from the PACK magic would
            // include pkt-line headers and corrupt the pack.
            let mut pack: Vec<u8> = Vec::new();
            let mut offset = 0usize;
            while offset + 4 <= bytes.len() {
                let header = String::from_utf8_lossy(&bytes[offset..offset + 4]).to_string();
                offset += 4;
                if header == "0000" {
                    continue;
                }
                let Ok(len) = usize::from_str_radix(&header, 16) else {
                    break;
                };
                if len < 4 || offset + len - 4 > bytes.len() {
                    continue;
                }
                let payload = &bytes[offset..offset + len - 4];
                offset += len - 4;
                if payload.first() == Some(&1) {
                    pack.extend_from_slice(&payload[1..]);
                }
            }
            assert!(
                pack.starts_with(b"PACK"),
                "crafted shallow request returned no pack (status {status})"
            );
            pack
        }

        /// Crafted v2 fetch combining a pack filter with include-tag. Used to prove
        /// that an authorised tag on a *retained* tree still arrives when blobs are
        /// withheld.
        async fn crafted_filtered_include_tag_pack_v2(
            &self,
            path: &str,
            want_oid: &str,
            filter: &str,
        ) -> Vec<u8> {
            let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4);
            let body = format!(
                "{}{}0001{}{}{}{}0000",
                pkt("command=fetch\n"),
                pkt("agent=git/2\n"),
                pkt(&format!("want {want_oid}\n")),
                pkt(&format!("filter {filter}\n")),
                pkt("include-tag\n"),
                pkt("done\n")
            );
            let response = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    self.port,
                    &format!("{path}/git-upload-pack"),
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .header("git-protocol", "version=2")
                .body(body)
                .send()
                .await
                .expect("crafted filtered v2 request");
            let status = response.status();
            let bytes = response.bytes().await.expect("crafted filtered v2 body");
            let start = bytes
                .windows(8)
                .position(|window| window == b"packfile")
                .unwrap_or_else(|| {
                    panic!(
                        "filtered include-tag response had no packfile section (status {status})"
                    )
                });
            let mut pack: Vec<u8> = Vec::new();
            let mut offset = start - 4;
            while offset + 4 <= bytes.len() {
                let header = String::from_utf8_lossy(&bytes[offset..offset + 4]).to_string();
                offset += 4;
                if header == "0000" {
                    break;
                }
                let Ok(len) = usize::from_str_radix(&header, 16) else {
                    break;
                };
                if len < 4 || offset + len - 4 > bytes.len() {
                    continue;
                }
                let payload = &bytes[offset..offset + len - 4];
                offset += len - 4;
                if payload.first() == Some(&1) {
                    pack.extend_from_slice(&payload[1..]);
                }
            }
            pack
        }

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

        async fn ssh_none_authenticated(&self, user: &str) -> bool {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut session = russh::client::connect(
                    std::sync::Arc::new(russh::client::Config::default()),
                    ("127.0.0.1", self.port),
                    SshClient,
                )
                .await
                .expect("connect fresh SSH none-auth probe");
                let accepted = session
                    .authenticate_none(user)
                    .await
                    .expect("receive SSH none-auth response")
                    .success();
                session
                    .disconnect(russh::Disconnect::ByApplication, "probe complete", "en")
                    .await
                    .expect("disconnect SSH none-auth probe");
                assert!(
                    matches!(session.await, Ok(()) | Err(russh::Error::Disconnect)),
                    "SSH none-auth probe task failed during close"
                );
                accepted
            })
            .await
            .expect("SSH none-auth probe deadline")
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
            let service = if receive {
                "git-receive-pack"
            } else {
                "git-upload-pack"
            };
            // This is an ordinary code-login key with no trace credential
            // binding. Enabling traces must not authorize it or advertise a
            // scope. MTR39's separate bound-owner probes cover true admission.
            ssh_refused(
                &session,
                &format!("{service} '/.traces/by-scope/{}.git'", self.scope),
                false,
            )
            .await;
            unchanged(&before, &snapshot(db).await);
            close_scope_session(session).await;
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

    // MTR39 drives only the public binary and SSH transport. These identities
    // are independent: a Cedar account is not a trace principal or a key id.
    const SCOPE_WRITER: &str = "mtr39-project-writer";
    const SCOPE_NONWRITER: &str = "mtr39-project-nonwriter";

    struct ScopeKey {
        id: i64,
        path: PathBuf,
        public: String,
        fingerprint: String,
    }

    struct WrongScopeSigner {
        wrong: std::sync::Arc<russh::keys::PrivateKey>,
        called: bool,
    }

    impl russh::Signer for WrongScopeSigner {
        type Error = russh::AgentAuthError;

        async fn auth_sign(
            &mut self,
            _key: &russh::keys::agent::AgentIdentity,
            _hash_alg: Option<russh::keys::HashAlg>,
            mut data: Vec<u8>,
        ) -> Result<Vec<u8>, Self::Error> {
            use russh::keys::ssh_encoding::Encode;
            self.called = true;
            let signature: russh::keys::ssh_key::Signature =
                russh::keys::signature::Signer::try_sign(self.wrong.as_ref(), &data)
                    .map_err(|_| russh::keys::Error::InvalidSignature)?;
            let mut encoded_signature = Vec::new();
            signature
                .encode(&mut encoded_signature)
                .map_err(russh::keys::Error::from)?;
            encoded_signature
                .encode(&mut data)
                .map_err(russh::keys::Error::from)?;
            Ok(data)
        }
    }

    struct ScopeAuthFixture {
        fixture: Fixture,
        http_port: u16,
        owner: String,
        outsider: String,
        next_key_id: i64,
    }

    impl ScopeAuthFixture {
        async fn new(db: &sea_orm::DatabaseConnection) -> Self {
            Self::new_with_kind(db, "sha1").await
        }

        async fn new_with_kind(db: &sea_orm::DatabaseConnection, kind: &str) -> Self {
            assert!(matches!(kind, "sha1" | "sha256"));
            let fixture = Fixture::new(db).await;
            let config_path = fixture.temp.path().join("config.toml");
            let mut config: toml::Value =
                toml::from_str(&fs::read_to_string(&config_path).expect("scope config"))
                    .expect("parse scope config");
            config["monorepo"]["admin"] =
                toml::Value::Array(vec![toml::Value::String(SCOPE_WRITER.to_owned())]);
            fs::write(
                &config_path,
                toml::to_string(&config).expect("scope config TOML"),
            )
            .expect("write isolated scope config");
            // Init writes the real main /.mega_cedar.json with this account as
            // admin. No synthetic policy snapshot or private auth API is used.
            let mut init = fixture.command(false, "trunk", "/third-party");
            init.args(["service", "init", "--yes"]);
            OwnedChild::spawn(&mut init).require_success().await;
            let owner = Uuid::new_v4().to_string();
            let outsider = Uuid::new_v4().to_string();
            assert_ne!(owner, outsider);
            for id in [&owner, &outsider] {
                trace_principal::ActiveModel {
                    id: Set(id.clone()),
                    created_at: Set(Utc::now().fixed_offset()),
                }
                .insert(db)
                .await
                .expect("seed independent scope principal");
            }
            let target = Uuid::new_v4().to_string();
            trace_target::ActiveModel {
                id: Set(target.clone()),
                target_kind: Set("native".to_owned()),
                project_path: Set("/project".to_owned()),
                backing_import_repo_id: Set(None),
                active: Set(true),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .expect("seed real project target");
            trace_stream::ActiveModel {
                id: Set(fixture.scope.clone()),
                principal_id: Set(owner.clone()),
                target_id: Set(target),
                source_repo_id: Set("mtr39-auth-only".to_owned()),
                epoch_id: Set(fixture.epoch.clone()),
                hash_kind: Set(kind.to_owned()),
                tip_oid: Set(None),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .expect("seed owner scope without PACK");
            Self {
                fixture,
                http_port: 0,
                owner,
                outsider,
                next_key_id: Utc::now().timestamp_micros(),
            }
        }

        async fn key(&mut self, username: &str) -> ScopeKey {
            self.next_key_id += 1;
            let id = self.next_key_id;
            let path = self.fixture.shared.path().join(format!("ssh/scope-{id}"));
            git_cli::generate_client_ed25519(&path);
            let public = fs::read_to_string(path.with_extension("pub")).expect("scope public key");
            let fingerprint = git_cli::ssh_fingerprint_sha256_col2(&path.with_extension("pub"));
            self.fixture.db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "INSERT INTO ssh_keys (id, username, title, ssh_key, finger, created_at) VALUES ($1,$2,$3,$4,$5,now())",
                [id.into(), username.into(), "MTR39 disposable key".into(), public.clone().into(), fingerprint.clone().into()],
            )).await.expect("seed source SSH key");
            let actual: i64 = self
                .fixture
                .db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT id FROM ssh_keys WHERE id=$1",
                    [id.into()],
                ))
                .await
                .expect("read actual key id")
                .expect("source key row")
                .try_get("", "id")
                .expect("source row id");
            assert_eq!(actual, id);
            ScopeKey {
                id: actual,
                path,
                public,
                fingerprint,
            }
        }

        async fn bind(&self, principal: &str, namespace: &str, credential_id: &str) {
            trace_credential_binding::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                principal_id: Set(principal.to_owned()),
                namespace: Set(namespace.to_owned()),
                credential_id: Set(credential_id.to_owned()),
                created_at: Set(Utc::now().fixed_offset()),
                retired_at: Set(None),
            }
            .insert(&self.fixture.db)
            .await
            .expect("bind actual credential namespace");
        }

        fn static_token(&self, name: &str, path: &str) -> String {
            let token = format!("mtr39-disposable-{}", Uuid::new_v4());
            let config_path = self.fixture.temp.path().join("config.toml");
            let mut config = fs::read_to_string(&config_path).expect("static scope config");
            config.push_str(&format!(
                "\n[[git.push_tokens]]\nname = {name:?}\ntoken = {token:?}\npaths = [{path:?}]\n"
            ));
            fs::write(config_path, config).expect("configure disposable static token");
            token
        }

        async fn start(&mut self, storage_only: bool, token: bool, enforcement: &str) {
            self.stop();
            self.fixture.traces_auth = true;
            self.fixture.static_auth = token;
            self.http_port = git_cli::reserve_ephemeral_port();
            self.fixture.port = git_cli::reserve_ephemeral_port();
            git_cli::record_allocated_port(self.http_port);
            git_cli::record_allocated_port(self.fixture.port);
            let mut command = self.fixture.command(
                true,
                if storage_only { "trunk" } else { "review" },
                "/third-party",
            );
            command
                .env("MEGA_CEDAR__ENFORCEMENT", enforcement)
                .env("MEGA_GIT__SSH_RECEIVE_PACK", "false")
                .env("MEGA_GIT_OBJECT_CACHE_PREFIX", "disabled");
            git_cli::apply_mega2_public_http_base_env(&mut command, self.http_port);
            command
                .args([
                    "service",
                    "multi",
                    "http",
                    "ssh",
                    "--host",
                    git_cli::service_listen_host(),
                    "-p",
                ])
                .arg(self.http_port.to_string())
                .arg("--ssh-port")
                .arg(self.fixture.port.to_string());
            self.fixture.service = OwnedChild::spawn(&mut command);
            git_cli::record_service_pid(
                self.fixture
                    .service
                    .0
                    .as_ref()
                    .expect("scope multi child")
                    .id(),
            );
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("scope readiness client");
            tokio::time::timeout(LIMIT, async {
                loop {
                    assert!(self.fixture.service.0.as_mut().expect("scope multi child").try_wait().expect("poll scope multi").is_none(), "scope multi exited before readiness");
                    let http = matches!(client.get(git_cli::mega2_host_http_url(self.http_port, "/api/openapi.json")).send().await, Ok(response) if response.status() == reqwest::StatusCode::OK);
                    let ssh = if let Ok(mut socket) = tokio::net::TcpStream::connect(("127.0.0.1", self.fixture.port)).await {
                        let mut banner = [0;4];
                        matches!(tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut banner)).await, Ok(Ok(_))) && banner == *b"SSH-"
                    } else { false };
                    if http && ssh && self.fixture.initial_index_ready().await { break; }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }).await.expect("scope shared HTTP/SSH readiness deadline");
        }

        fn stop(&mut self) {
            self.fixture.stop();
            if self.http_port != 0 {
                git_cli::wait_until_port_closed(self.http_port, Duration::from_secs(5));
                self.http_port = 0;
            }
        }

        async fn connect(&self) -> SshSession {
            let (session,) = tokio::time::timeout(Duration::from_secs(10), async {
                (russh::client::connect(
                    std::sync::Arc::new(russh::client::Config::default()),
                    ("127.0.0.1", self.fixture.port),
                    SshClient,
                )
                .await
                .expect("fresh scope SSH connection"),)
            })
            .await
            .expect("scope SSH connection deadline");
            session
        }

        async fn key_session(&self, login: &str, key: &ScopeKey, expected: bool) -> SshSession {
            let mut session = self.connect().await;
            let key = russh::keys::load_secret_key(&key.path, None).expect("scope private key");
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                session.authenticate_publickey(
                    login,
                    russh::keys::PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), None),
                ),
            )
            .await
            .expect("scope signed-key deadline")
            .expect("scope signed-key response");
            assert_eq!(
                result.success(),
                expected,
                "real key authentication result for requested login"
            );
            session
        }

        async fn password_session(&self, password: &str, expected: bool) -> SshSession {
            let mut session = self.connect().await;
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                session.authenticate_password("traces", password),
            )
            .await
            .expect("scope password deadline")
            .expect("scope password response");
            assert_eq!(
                result.success(),
                expected,
                "real static-password authentication result"
            );
            session
        }

        fn receive_command(&self) -> String {
            format!(
                "git-receive-pack '/.traces/by-scope/{}.git'",
                self.fixture.scope
            )
        }

        async fn receive_allowed(&self, session: &SshSession) {
            let before = snapshot(&self.fixture.db).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut channel = session
                    .channel_open_session()
                    .await
                    .expect("authorized scope channel");
                channel
                    .exec(true, self.receive_command())
                    .await
                    .expect("authorized receive exec");
                let mut data = Vec::new();
                let mut accepted = false;
                while let Some(message) = channel.wait().await {
                    match message {
                        russh::ChannelMsg::Success => accepted = true,
                        russh::ChannelMsg::Data { data: bytes } => {
                            data.extend_from_slice(&bytes);
                            assert!(data.len() < 64 * 1024, "bounded scope advertisement");
                        }
                        russh::ChannelMsg::ExtendedData { data, .. } => assert!(
                            data.is_empty(),
                            "authorized receive cannot use code-write refusal"
                        ),
                        russh::ChannelMsg::Failure | russh::ChannelMsg::Close => {
                            panic!("authorized TraceRepo receive was refused")
                        }
                        _ => {}
                    }
                    if accepted && data.ends_with(b"0000") {
                        break;
                    }
                }
                assert!(
                    accepted && data.ends_with(b"0000"),
                    "real exec success and trace advertisement required"
                );
                assert!(
                    String::from_utf8_lossy(&data).contains("capabilities^{}"),
                    "empty seeded TraceRepo advertisement"
                );
                assert!(!data.windows(4).any(|part| part == b"PACK"));
                assert!(!String::from_utf8_lossy(&data).contains("refs/heads/main"));
                // No PACK/data-plane assertion belongs to this card.
                channel.eof().await.expect("finish auth-only receive");
                channel.close().await.expect("close auth-only receive");
            })
            .await
            .expect("authorized trace receive advertisement deadline");
            unchanged(&before, &snapshot(&self.fixture.db).await);
        }

        async fn receive_refused(&self, session: &SshSession) {
            let before = snapshot(&self.fixture.db).await;
            ssh_refused(session, &self.receive_command(), false).await;
            ssh_refused(
                session,
                &format!(
                    "git-receive-pack '/.traces/by-scope/{}.git'",
                    Uuid::new_v4()
                ),
                false,
            )
            .await;
            unchanged(&before, &snapshot(&self.fixture.db).await);
        }

        async fn code_receive_refused(&self, session: &SshSession) {
            let before = snapshot(&self.fixture.db).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut channel = session
                    .channel_open_session()
                    .await
                    .expect("ordinary code receive channel");
                channel
                    .exec(true, "git-receive-pack '/project/control.git'")
                    .await
                    .expect("ordinary code receive exec");
                let mut stderr = Vec::new();
                let mut eof = false;
                let mut close = false;
                while let Some(message) = channel.wait().await {
                    match message {
                        russh::ChannelMsg::ExtendedData { data, ext } => {
                            assert_eq!(ext, 1);
                            stderr.extend_from_slice(&data);
                        }
                        russh::ChannelMsg::Data { data } => {
                            assert!(data.is_empty(), "ordinary code receive must not advertise")
                        }
                        russh::ChannelMsg::Failure => {
                            panic!("ordinary receive requires existing disabled oracle")
                        }
                        russh::ChannelMsg::Eof => eof = true,
                        russh::ChannelMsg::Close => {
                            close = true;
                            break;
                        }
                        _ => {}
                    }
                }
                assert_eq!(stderr, b"error: SSH receive-pack is disabled\n");
                assert!(eof && close, "ordinary receive disabled EOF/close");
            })
            .await
            .expect("ordinary code receive refused before stdin");
            unchanged(&before, &snapshot(&self.fixture.db).await);
        }
    }

    async fn close_scope_session(session: SshSession) {
        tokio::time::timeout(Duration::from_secs(10), async {
            session
                .disconnect(
                    russh::Disconnect::ByApplication,
                    "scope probe complete",
                    "en",
                )
                .await
                .expect("disconnect scope probe");
            assert!(
                matches!(session.await, Ok(()) | Err(russh::Error::Disconnect)),
                "scope transport close failed"
            );
        })
        .await
        .expect("scope transport close deadline");
    }

    async fn discovery_wire_curl_v0(
        fixture: &ScopeAuthFixture,
        token: Option<&str>,
        label: &str,
    ) -> Response {
        wire_http_request(
            fixture,
            token,
            label,
            &format!("/.traces/by-scope/{}.git", fixture.fixture.scope),
            false,
            false,
            None,
        )
        .await
    }

    async fn wire_http_request(
        fixture: &ScopeAuthFixture,
        token: Option<&str>,
        label: &str,
        locator: &str,
        v2: bool,
        receive: bool,
        request_body: Option<&[u8]>,
    ) -> Response {
        let config_path = fixture
            .fixture
            .temp
            .path()
            .join(format!("{label}.curl-config"));
        let body_path = fixture
            .fixture
            .temp
            .path()
            .join(format!("{label}.curl-body"));
        let header_path = fixture
            .fixture
            .temp
            .path()
            .join(format!("{label}.curl-headers"));
        let stderr_path = fixture
            .fixture
            .temp
            .path()
            .join(format!("{label}.curl-stderr"));
        let service = if receive {
            "git-receive-pack"
        } else {
            "git-upload-pack"
        };
        let endpoint = if request_body.is_some() {
            format!("{locator}/{service}")
        } else {
            format!("{locator}/info/refs?service={service}")
        };
        let url = format!("http://127.0.0.1:{}{endpoint}", fixture.http_port);
        let method = if request_body.is_some() {
            "POST"
        } else {
            "GET"
        };
        let mut config = format!(
            "url = \"{url}\"\nrequest = \"{method}\"\nhttp1.1\nsilent\nshow-error\nconnect-timeout = 2\nmax-time = 10\nmax-filesize = 1048575\nproxy = \"\"\nnoproxy = \"*\"\n"
        );
        if let Some(token) = token {
            config.push_str(&format!("header = \"Authorization: Bearer {token}\"\n"));
        }
        if v2 {
            config.push_str("header = \"Git-Protocol: version=2\"\n");
        }
        if let Some(body) = request_body {
            let request_path = fixture
                .fixture
                .temp
                .path()
                .join(format!("{label}.curl-request"));
            fs::write(&request_path, body).expect("private curl request bytes");
            config.push_str(&format!("header = \"Content-Type: application/x-{service}-request\"\ndata-binary = \"@{}\"\n", request_path.display()));
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut private_config = options
            .open(&config_path)
            .expect("private curl configuration");
        std::io::Write::write_all(&mut private_config, config.as_bytes())
            .expect("write private curl configuration");
        drop(private_config);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&config_path)
                    .expect("curl config metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "curl credential config must be private"
            );
        }
        // -q is deliberately the first argument. No curlrc, proxy environment,
        // credential in argv/URL, verbose trace, or inherited auth is used.
        let mut command = Command::new("/usr/bin/curl");
        command
            .env_clear()
            .current_dir(fixture.fixture.temp.path())
            .args(["-q", "--config"])
            .arg(&config_path)
            .arg("--output")
            .arg(&body_path)
            .arg("--dump-header")
            .arg(&header_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(&stderr_path).expect("private curl stderr"),
            ));
        let mut child = OwnedChild::spawn(&mut command);
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child
                .0
                .as_mut()
                .expect("owned curl")
                .try_wait()
                .expect("poll curl")
            {
                child.0.take();
                break status;
            }
            for (path, limit) in [(&body_path, 1024 * 1024), (&header_path, 64 * 1024)] {
                if let Ok(metadata) = fs::metadata(path) {
                    assert!(metadata.len() < limit, "bounded curl discovery output");
                }
            }
            assert!(Instant::now() < deadline, "curl discovery exceeded 10s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(
            status.success(),
            "standard curl discovery process failed: {status}"
        );
        let headers = fs::read_to_string(header_path).expect("curl HTTP headers");
        let body = fs::read(body_path).expect("curl raw pkt-line body");
        assert!(headers.len() < 64 * 1024 && body.len() < 1024 * 1024);
        let status = headers
            .lines()
            .next()
            .expect("curl status line")
            .split_whitespace()
            .nth(1)
            .expect("curl status value")
            .parse()
            .expect("numeric curl HTTP status");
        Response {
            status,
            headers,
            body,
        }
    }

    fn discovery_wire_packets(mut bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        while !bytes.is_empty() {
            assert!(
                bytes.len() >= 4 && bytes[..4].iter().all(u8::is_ascii_hexdigit),
                "complete Git pkt-line header"
            );
            let size =
                usize::from_str_radix(std::str::from_utf8(&bytes[..4]).expect("pkt-line hex"), 16)
                    .expect("pkt-line length");
            if size == 0 {
                packets.push(Vec::new());
                bytes = &bytes[4..];
            } else {
                assert!(
                    (5..=65520).contains(&size) && size <= bytes.len(),
                    "complete bounded Git pkt-line"
                );
                packets.push(bytes[4..size].to_vec());
                bytes = &bytes[size..];
            }
        }
        packets
    }

    pub async fn http_v0_metadata_discovery_probe() {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(ScopeAuthFixture::new(&db).await);
            let fixture = fixture.as_mut().expect("HTTP v0 discovery fixture");
            let token = fixture.static_token("mtr92-discovery-owner", "/project");
            fixture
                .bind(&fixture.owner, "static_push_token", "mtr92-discovery-owner")
                .await;
            fixture.start(true, true, "off").await;
            let before = snapshot(&db).await;
            let unauthenticated =
                discovery_wire_curl_v0(fixture, None, "mtr92-unauthenticated").await;
            assert_eq!(
                unauthenticated.status, 401,
                "the same real discovery requires authentication"
            );
            assert!(unauthenticated.body.is_empty());
            let authorized =
                discovery_wire_curl_v0(fixture, Some(&token), "mtr92-authorized").await;
            assert_eq!(
                authorized.status, 200,
                "current bound owner must receive real HTTP discovery"
            );
            assert!(
                authorized
                    .headers
                    .to_ascii_lowercase()
                    .contains("application/x-git-upload-pack-advertisement")
            );
            let packets = discovery_wire_packets(&authorized.body);
            assert_eq!(
                packets.len(),
                4,
                "HTTP v0 service, flush, one empty ref, flush"
            );
            assert_eq!(packets[0], b"# service=git-upload-pack\n");
            assert!(packets[1].is_empty() && packets[3].is_empty());
            assert!(
                packets[2]
                    .starts_with(format!("{} capabilities^{{}}\0", "0".repeat(40)).as_bytes()),
                "real existing empty SHA1 scope advertisement"
            );
            let line = std::str::from_utf8(&packets[2]).expect("UTF8 v0 advertisement");
            let (_, capabilities) = line.split_once('\0').expect("v0 capabilities follow NUL");
            assert!(
                capabilities
                    .split_ascii_whitespace()
                    .any(|cap| cap == "object-format=sha1")
            );
            unchanged(&before, &snapshot(&db).await);
            assert!(
                capabilities
                    .split_ascii_whitespace()
                    .any(|cap| cap == "libra-traces-v1"),
                "MTR92_WIRE_ORACLE: missing libra-traces-v1 on authenticated HTTP v0 discovery"
            );
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
            let logs = fixture.fixture.temp.keep();
            for name in [
                "process.out",
                "process.err",
                "mtr92-authorized.curl-body",
                "mtr92-authorized.curl-headers",
                "mtr92-authorized.curl-stderr",
            ] {
                if let Ok(bytes) = fs::read(logs.join(name)) {
                    eprintln!(
                        "HTTP v0 discovery failure artifact: file={} sha256={}",
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
        closed.expect("close discovery pool before database guard");
    }

    #[derive(Debug, PartialEq, Eq)]
    struct WireMetadata {
        server: String,
        scope: Option<String>,
        format: String,
    }

    struct WireExpected<'a> {
        locator: &'a str,
        kind: &'a str,
        scope: Option<&'a str>,
        v2: bool,
        receive: bool,
    }

    fn wire_metadata(bytes: &[u8], http: bool, expected: &WireExpected<'_>) -> WireMetadata {
        wire_metadata_with_tip(bytes, http, expected, None)
    }

    fn wire_metadata_with_tip(
        bytes: &[u8],
        http: bool,
        expected: &WireExpected<'_>,
        tip: Option<&str>,
    ) -> WireMetadata {
        let mut packets = discovery_wire_packets(bytes);
        if http && !expected.v2 {
            let service = if expected.receive {
                "git-receive-pack"
            } else {
                "git-upload-pack"
            };
            assert_eq!(
                packets.remove(0),
                format!("# service={service}\n").as_bytes()
            );
            assert!(packets.remove(0).is_empty(), "HTTP service flush");
        }
        assert!(packets.pop().expect("advertisement flush").is_empty());
        let capabilities: Vec<String> = if expected.v2 {
            assert!(!expected.receive, "Git v2 is upload discovery only");
            let lines: Vec<_> = packets
                .into_iter()
                .map(|line| String::from_utf8(line).expect("v2 UTF8 capability"))
                .collect();
            assert_eq!(lines.first().map(String::as_str), Some("version 2\n"));
            assert_eq!(lines.get(1).map(String::as_str), Some("agent=mega/0.1.0\n"));
            assert_eq!(lines.get(2).map(String::as_str), Some("ls-refs\n"));
            assert_eq!(lines.get(3).map(String::as_str), Some("fetch\n"));
            assert!(lines.iter().all(|line| line.ends_with('\n')));
            lines
                .into_iter()
                .skip(1)
                .map(|line| line.trim_end_matches('\n').to_owned())
                .collect()
        } else {
            assert_eq!(packets.len(), 1, "one seeded empty trace ref");
            let line = std::str::from_utf8(&packets[0]).expect("v0 UTF8 trace ref");
            let (reference, capabilities) =
                line.split_once('\0').expect("v0 NUL capability separator");
            let width = if expected.kind == "sha1" { 40 } else { 64 };
            let expected_reference = match tip {
                Some(tip) => {
                    assert_eq!(tip.len(), width, "actual tip has authorized hash width");
                    format!("{tip} refs/libra/traces")
                }
                None => format!("{} capabilities^{{}}", "0".repeat(width)),
            };
            assert_eq!(
                reference, expected_reference,
                "authorized metadata and actual tip"
            );
            assert!(capabilities.ends_with('\n'));
            capabilities
                .split_ascii_whitespace()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(
            capabilities
                .iter()
                .filter(|cap| cap.as_str() == "libra-traces-v1")
                .count(),
            1
        );
        let values = |prefix: &str| {
            capabilities
                .iter()
                .filter_map(|cap| cap.strip_prefix(prefix).map(str::to_owned))
                .collect::<Vec<_>>()
        };
        let servers = values("libra-traces-server=");
        assert_eq!(servers, [TRACE_SERVER_ID]);
        let scopes = values("libra-traces-scope=");
        assert_eq!(
            scopes,
            expected
                .scope
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "only the actual existing stream UUID is advertised"
        );
        assert_eq!(values("object-format="), [expected.kind]);
        for unsupported in [
            "shallow",
            "filter",
            "ofs-delta",
            "delete-refs",
            "side-band-64k",
            "fetch=shallow filter",
        ] {
            assert!(
                !capabilities.iter().any(|cap| cap == unsupported),
                "traces advertises only implemented capabilities"
            );
        }
        if !expected.v2 {
            assert_eq!(
                capabilities.iter().any(|cap| cap == "report-status"),
                expected.receive
            );
            assert_eq!(
                capabilities.iter().any(|cap| cap == "no-thin"),
                expected.receive
            );
        }
        if expected.v2 {
            let mut exact = vec![
                "agent=mega/0.1.0".to_owned(),
                "ls-refs".to_owned(),
                "fetch".to_owned(),
                format!("object-format={}", expected.kind),
                "libra-traces-v1".to_owned(),
                format!("libra-traces-server={TRACE_SERVER_ID}"),
            ];
            if let Some(scope) = expected.scope {
                exact.push(format!("libra-traces-scope={scope}"));
            }
            assert_eq!(capabilities, exact, "exact authorized v2 capability lines");
        }
        WireMetadata {
            server: servers[0].clone(),
            scope: scopes.into_iter().next(),
            format: expected.kind.to_owned(),
        }
    }

    async fn wire_ssh_advertisement(
        session: &SshSession,
        locator: &str,
        v2: bool,
        receive: bool,
    ) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut channel = session
                .channel_open_session()
                .await
                .expect("wire discovery SSH channel");
            if v2 {
                channel
                    .set_env(false, "GIT_PROTOCOL", "version=2")
                    .await
                    .expect("wire SSH Git protocol env");
            }
            let service = if receive {
                "git-receive-pack"
            } else {
                "git-upload-pack"
            };
            channel
                .exec(true, format!("{service} '{locator}'"))
                .await
                .expect("wire SSH discovery exec");
            let mut bytes = Vec::new();
            let mut accepted = false;
            while let Some(message) = channel.wait().await {
                match message {
                    russh::ChannelMsg::Success => accepted = true,
                    russh::ChannelMsg::Data { data } => {
                        bytes.extend_from_slice(&data);
                        assert!(bytes.len() < 64 * 1024, "bounded SSH discovery");
                    }
                    russh::ChannelMsg::ExtendedData { data, .. } => assert!(
                        data.is_empty(),
                        "discovery cannot use generic SSH error output"
                    ),
                    russh::ChannelMsg::Failure | russh::ChannelMsg::Close => {
                        panic!("authorized SSH discovery refused")
                    }
                    _ => {}
                }
                if accepted && bytes.ends_with(b"0000") {
                    break;
                }
            }
            assert!(
                accepted && bytes.ends_with(b"0000"),
                "actual exec success and complete advertisement required"
            );
            channel
                .eof()
                .await
                .expect("finish discovery-only SSH request");
            channel
                .close()
                .await
                .expect("close discovery-only SSH channel");
            bytes
        })
        .await
        .expect("wire SSH advertisement deadline")
    }

    async fn wire_expect_metadata(
        fixture: &ScopeAuthFixture,
        token: &str,
        ssh: Option<&SshSession>,
        expected: WireExpected<'_>,
    ) -> WireMetadata {
        let before = snapshot(&fixture.fixture.db).await;
        let response = wire_http_request(
            fixture,
            Some(token),
            &format!("wire-{}", Uuid::new_v4()),
            expected.locator,
            expected.v2,
            expected.receive,
            None,
        )
        .await;
        assert_eq!(response.status, 200, "authorized actual curl discovery");
        let service = if expected.receive {
            "git-receive-pack"
        } else {
            "git-upload-pack"
        };
        assert!(
            response
                .headers
                .to_ascii_lowercase()
                .contains(&format!("application/x-{service}-advertisement"))
        );
        let http = wire_metadata(&response.body, true, &expected);
        if let Some(session) = ssh {
            let bytes =
                wire_ssh_advertisement(session, expected.locator, expected.v2, expected.receive)
                    .await;
            assert_eq!(
                wire_metadata(&bytes, false, &expected),
                http,
                "same configured server and actual stream on HTTP/SSH"
            );
        }
        unchanged(&before, &snapshot(&fixture.fixture.db).await);
        http
    }

    async fn wire_expect_refusal(
        fixture: &ScopeAuthFixture,
        token: Option<&str>,
        ssh: Option<&SshSession>,
        locator: &str,
        v2: bool,
        receive: bool,
        status: u16,
    ) -> Vec<u8> {
        let before = snapshot(&fixture.fixture.db).await;
        let response = wire_http_request(
            fixture,
            token,
            &format!("wire-denied-{}", Uuid::new_v4()),
            locator,
            v2,
            receive,
            None,
        )
        .await;
        assert_eq!(
            response.status, status,
            "actual discovery refusal, not setup/transport failure"
        );
        assert!(
            !response
                .body
                .windows(b"libra-traces".len())
                .any(|part| part == b"libra-traces")
        );
        if status == 401 {
            assert!(response.body.is_empty());
        }
        if let Some(session) = ssh {
            let service = if receive {
                "git-receive-pack"
            } else {
                "git-upload-pack"
            };
            ssh_refused(session, &format!("{service} '{locator}'"), v2).await;
        }
        unchanged(&before, &snapshot(&fixture.fixture.db).await);
        response.body
    }

    fn wire_by_path(target: &str, source: &str, epoch: &str, kind: &str) -> String {
        format!(
            "/.traces/by-path/{}/self/{}/{epoch}/{kind}.git",
            URL_SAFE_NO_PAD.encode(target),
            URL_SAFE_NO_PAD.encode(source)
        )
    }

    async fn wire_access_token(
        fixture: &mut ScopeAuthFixture,
        username: &str,
        principal: &str,
    ) -> String {
        fixture.next_key_id += 1;
        let id = fixture.next_key_id;
        let token = format!("mtr92-disposable-{}", Uuid::new_v4());
        mega2_core::access_token::ActiveModel {
            id: Set(id),
            username: Set(username.to_owned()),
            token: Set(token.clone()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(&fixture.fixture.db)
        .await
        .expect("current real Mono access token");
        fixture
            .bind(principal, "mono_account", &format!("access-token:{id}"))
            .await;
        token
    }

    async fn wire_code_control(fixture: &ScopeAuthFixture, ssh: Option<&SshSession>, v2: bool) {
        let before = snapshot(&fixture.fixture.db).await;
        let response = wire_http_request(
            fixture,
            None,
            &format!("wire-code-{}", Uuid::new_v4()),
            "/.git",
            v2,
            false,
            None,
        )
        .await;
        assert_eq!(response.status, 200);
        let mut advertisements = vec![response.body];
        if let Some(session) = ssh {
            advertisements.push(wire_ssh_advertisement(session, "/.git", v2, false).await);
        }
        for bytes in advertisements {
            assert!(
                !bytes
                    .windows(b"libra-traces".len())
                    .any(|part| part == b"libra-traces"),
                "ordinary code has no traces capabilities"
            );
            let packets = discovery_wire_packets(&bytes);
            assert!(packets.last().expect("code advertisement flush").is_empty());
            if v2 {
                assert_eq!(packets[0], b"version 2\n");
                assert!(packets.iter().any(|line| line == b"fetch=shallow filter\n"));
            } else {
                assert!(
                    bytes
                        .windows(b"refs/heads/main".len())
                        .any(|part| part == b"refs/heads/main"),
                    "positive ordinary code refs remain"
                );
            }
            assert!(
                bytes
                    .windows(b"object-format=sha1".len())
                    .any(|part| part == b"object-format=sha1")
            );
        }
        unchanged(&before, &snapshot(&fixture.fixture.db).await);
    }

    async fn wire_standard_push_status(fixture: &ScopeAuthFixture, token: &str, kind: &str) {
        let git = &fixture.fixture;
        git.git(&[
            "init",
            "--initial-branch=main",
            &format!("--object-format={kind}"),
            "trace-source",
        ]);
        git.git(&["-C", "trace-source", "config", "pack.window", "0"]);
        git.git(&["-C", "trace-source", "config", "gc.auto", "0"]);
        let checkpoint = git.shared.path().join("trace-source/checkpoint/ab/cd");
        fs::create_dir_all(&checkpoint).expect("standard status checkpoint");
        fs::write(
            checkpoint.join("metadata.json"),
            b"{\"schema_version\":2}\n",
        )
        .expect("standard status metadata");
        fs::write(
            checkpoint.join("raw.bin"),
            b"MTR92 standard report-status fixture\n",
        )
        .expect("standard status raw capture");
        let tip = http_git_commit(git, "MTR92 standard push status");
        let width = if kind == "sha1" { 40 } else { 64 };
        assert_eq!(tip.len(), width);
        let pack = git.git(&[
            "-C",
            "trace-source",
            "pack-objects",
            "--stdout",
            "--revs",
            "--all",
            "--window=0",
            "--no-reuse-delta",
            "--no-reuse-object",
        ]);
        assert!(pack.starts_with(b"PACK"));
        let epoch = Uuid::new_v4().to_string();
        let locator = wire_by_path("/project", "mtr92-status-source", &epoch, kind);
        let mut request = Vec::new();
        pkt(
            &mut request,
            &format!(
                "{} {tip} refs/libra/traces\0report-status object-format={kind}\n",
                "0".repeat(width)
            ),
        );
        request.extend_from_slice(b"0000");
        request.extend_from_slice(&pack);
        // Only the already delivered HTTP data plane is used here. SSH PACK
        // roundtrips remain MTR12; both discovery selectors regress this status.
        let response = wire_http_request(
            fixture,
            Some(token),
            &format!("wire-status-{}", Uuid::new_v4()),
            &locator,
            false,
            true,
            Some(&request),
        )
        .await;
        assert_eq!(response.status, 200, "real standard report-status push");
        assert!(
            response
                .headers
                .to_ascii_lowercase()
                .contains("application/x-git-receive-pack-result")
        );
        assert_eq!(
            discovery_wire_packets(&response.body),
            vec![
                b"unpack ok\n".to_vec(),
                b"ok refs/libra/traces\n".to_vec(),
                Vec::new()
            ],
            "standard ok never carries custom metadata or UUIDs"
        );
        let stream = trace_stream::Entity::find()
            .filter(trace_stream::Column::EpochId.eq(epoch))
            .one(&git.db)
            .await
            .expect("actual standard push stream")
            .expect("standard push published a stream");
        assert_eq!(stream.principal_id, fixture.owner);
        assert_eq!(stream.tip_oid.as_deref(), Some(tip.as_str()));
        assert_eq!(stream.hash_kind, kind);
    }

    // MTR-15: the service process indexes a real Git push from the durable journal.
    pub async fn run_projection_worker() {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(ScopeAuthFixture::new(&db).await);
            let f = fixture.as_mut().expect("projection fixture");
            let key = f.key(SCOPE_WRITER).await;
            let name = format!("trace-ssh-key:{}", key.id);
            let token = f.static_token(&name, "/project");
            f.bind(&f.owner, "mono_account", &format!("ssh-key:{}", key.id))
                .await;
            f.bind(&f.owner, "static_push_token", &name).await;
            f.start(true, true, "off").await;
            let path = wire_by_path("/project", "mtr15-source", &f.fixture.epoch, "sha1");
            let shared = f.fixture.shared.path();
            let ssh_dir = shared.join("ssh");
            fs::create_dir_all(&ssh_dir).expect("projection SSH directory");
            fs::copy(&key.path, ssh_dir.join("client_ed25519")).expect("projection client key");
            git_cli::write_known_hosts_via_keyscan(&ssh_dir.join("known_hosts"), f.fixture.port);
            let ssh = git_cli::git_ssh_command(shared, f.fixture.port);
            let url = format!(
                "ssh://traces@{}:{}{}",
                git_cli::mega2_reachable_host(),
                f.fixture.port,
                path
            );
            f.fixture
                .git(&["init", "--object-format=sha1", "mtr15-source"]);
            let leaf = shared.join("mtr15-source/checkpoint/ab/cd");
            fs::create_dir_all(&leaf).expect("checkpoint leaf");
            fs::write(
                leaf.join("metadata.json"),
                br#"{"schema_version":9,"checkpoint_id":"abcd"}"#,
            )
            .expect("metadata");
            fs::write(
                leaf.join("manifest.json"),
                br#"{"schema_version":9,"checkpoint_id":"abcd"}"#,
            )
            .expect("manifest");
            f.fixture.git(&["-C", "mtr15-source", "add", "."]);
            f.fixture.git(&[
                "-C",
                "mtr15-source",
                "-c",
                "user.name=MTR15",
                "-c",
                "user.email=mtr15@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "checkpoint",
                "-m",
                "Libra-Parent-Commit: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ]);
            let tip = f
                .fixture
                .git_text(&["-C", "mtr15-source", "rev-parse", "HEAD"]);
            let push = git_cli::git_cli_ssh(
                shared,
                &ssh,
                &[
                    "-C",
                    "mtr15-source",
                    "push",
                    "--no-thin",
                    &url,
                    "HEAD:refs/libra/traces",
                ],
            );
            assert!(
                push.status.success(),
                "projection worker push failed: {}",
                String::from_utf8_lossy(&push.stderr)
            );
            let deadline = Instant::now() + Duration::from_secs(30);
            let (sessions, checkpoints) = loop {
                let state: Option<String> = db
                    .query_one_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "SELECT state FROM trace_ref_update WHERE new_tip = $1",
                        [tip.clone().into()],
                    ))
                    .await
                    .expect("journal query")
                    .and_then(|row| row.try_get("", "state").ok());
                let session: Option<String> = db
                    .query_one_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        "SELECT session_key FROM trace_session WHERE session_key = 'unsupported:abcd'".to_owned(),
                    ))
                    .await
                    .expect("session query")
                    .and_then(|row| row.try_get("", "session_key").ok());
                if state.as_deref() == Some("indexed") && session.as_deref() == Some("unsupported:abcd")
                {
                    let sessions: i64 = db
                        .query_one_raw(Statement::from_string(
                            DatabaseBackend::Postgres,
                            "SELECT count(*)::bigint AS n FROM trace_session".to_owned(),
                        ))
                        .await
                        .expect("session count")
                        .expect("row")
                        .try_get("", "n")
                        .expect("n");
                    let checkpoints: i64 = db
                        .query_one_raw(Statement::from_string(
                            DatabaseBackend::Postgres,
                            "SELECT count(*)::bigint AS n FROM trace_checkpoint".to_owned(),
                        ))
                        .await
                        .expect("checkpoint count")
                        .expect("row")
                        .try_get("", "n")
                        .expect("n");
                    break (sessions, checkpoints);
                }
                assert!(
                    Instant::now() < deadline,
                    "journal was not indexed from the durable push\nstderr:\n{}",
                    fs::read_to_string(f.fixture.temp.path().join("process.err")).unwrap_or_default()
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            let accepted: String = db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT tip_oid FROM trace_stream WHERE tip_oid = $1",
                    [tip.clone().into()],
                ))
                .await
                .expect("accepted tip")
                .expect("accepted tip row")
                .try_get("", "tip_oid")
                .expect("tip");
            assert_eq!(accepted, tip, "publication tip stays independent of indexing");
            f.stop();
            f.start(true, true, "off").await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            let sessions_after: i64 = db
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT count(*)::bigint AS n FROM trace_session".to_owned(),
                ))
                .await
                .expect("session count after restart")
                .expect("row")
                .try_get("", "n")
                .expect("n");
            let checkpoints_after: i64 = db
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT count(*)::bigint AS n FROM trace_checkpoint".to_owned(),
                ))
                .await
                .expect("checkpoint count after restart")
                .expect("row")
                .try_get("", "n")
                .expect("n");
            assert_eq!(sessions_after, sessions);
            assert_eq!(checkpoints_after, checkpoints);
            let _ = token;
        })
        .catch_unwind()
        .await;
        if let Some(fixture) = fixture.as_mut() {
            fixture.stop();
        }
        let closed = db.close().await;
        drop(guard);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        closed.expect("close projection worker pool");
    }

    // MTR12: real standard Git over the running binary and authorized SSH key.
    pub async fn run_ssh_scoped_token_key_roundtrip() {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(ScopeAuthFixture::new(&db).await);
            let f = fixture.as_mut().expect("FIX29 fixture");
            let key = f.key(SCOPE_WRITER).await;
            let name = format!("trace-ssh-key:{}", key.id);
            let token = f.static_token(&name, "/project");
            f.bind(&f.owner, "mono_account", &format!("ssh-key:{}", key.id))
                .await;
            f.bind(&f.owner, "static_push_token", &name).await;
            f.start(true, true, "off").await;
            let path = wire_by_path("/project", "fix29-source", &f.fixture.epoch, "sha1");
            let password = f.password_session(&token, true).await;
            wire_expect_metadata(
                f,
                &token,
                Some(&password),
                WireExpected {
                    locator: &path,
                    kind: "sha1",
                    scope: None,
                    v2: false,
                    receive: true,
                },
            )
            .await;
            close_scope_session(password).await;
            // The signed SSH key authenticates on the same live service before
            // standard Git proves its mapped path authorization.
            let key_session = f.key_session("traces", &key, true).await;
            close_scope_session(key_session).await;
            let shared = f.fixture.shared.path();
            let ssh_dir = shared.join("ssh");
            fs::create_dir_all(&ssh_dir).expect("FIX29 SSH directory");
            fs::copy(&key.path, ssh_dir.join("client_ed25519")).expect("FIX29 client key");
            git_cli::write_known_hosts_via_keyscan(&ssh_dir.join("known_hosts"), f.fixture.port);
            let ssh = git_cli::git_ssh_command(shared, f.fixture.port);
            let url = format!(
                "ssh://traces@{}:{}{}",
                git_cli::mega2_reachable_host(),
                f.fixture.port,
                path
            );
            f.fixture
                .git(&["init", "--object-format=sha1", "fix29-source"]);
            fs::write(
                shared.join("fix29-source/payload.bin"),
                b"FIX29 synthetic key scope\0\xff\n",
            )
            .expect("synthetic payload");
            f.fixture.git(&["-C", "fix29-source", "add", "payload.bin"]);
            f.fixture.git(&[
                "-C",
                "fix29-source",
                "-c",
                "user.name=FIX29",
                "-c",
                "user.email=fix29@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "key scope",
            ]);
            let tip = f
                .fixture
                .git_text(&["-C", "fix29-source", "rev-parse", "HEAD"]);
            let push = git_cli::git_cli_ssh(
                shared,
                &ssh,
                &[
                    "-C",
                    "fix29-source",
                    "push",
                    "--no-thin",
                    &url,
                    "HEAD:refs/libra/traces",
                ],
            );
            assert!(
                push.status.success(),
                "FIX29_REAL_KEY_PUSH_ORACLE: mapped signed-key Git push failed: {}",
                String::from_utf8_lossy(&push.stderr)
            );
            let stream = trace_stream::Entity::find()
                .filter(trace_stream::Column::PrincipalId.eq(f.owner.clone()))
                .filter(trace_stream::Column::SourceRepoId.eq("fix29-source"))
                .one(&db)
                .await
                .expect("FIX29 stream query")
                .expect("FIX29 stream created");
            assert_eq!(stream.tip_oid.as_deref(), Some(tip.as_str()));
            let scope = format!("/.traces/by-scope/{}.git", stream.id);
            let session = f.key_session("traces", &key, true).await;
            let response = wire_http_request(
                f,
                Some(&token),
                "fix29-after-push",
                &scope,
                false,
                false,
                None,
            )
            .await;
            assert_eq!(response.status, 200);
            let expected = WireExpected {
                locator: &scope,
                kind: "sha1",
                scope: Some(&stream.id),
                v2: false,
                receive: false,
            };
            let http = wire_metadata_with_tip(&response.body, true, &expected, Some(&tip));
            let advertisement = wire_ssh_advertisement(&session, &scope, false, false).await;
            assert_eq!(
                wire_metadata_with_tip(&advertisement, false, &expected, Some(&tip)),
                http
            );
            close_scope_session(session).await;
            // Fetch actual PACK through both authenticated transports and
            // compare the original raw bytes, not just advertisements.
            f.fixture
                .git(&["init", "--object-format=sha1", "fix29-ssh-fetch"]);
            let scoped_ssh = format!(
                "ssh://traces@{}:{}{}",
                git_cli::mega2_reachable_host(),
                f.fixture.port,
                scope
            );
            let fetched = git_cli::git_cli_ssh(
                shared,
                &ssh,
                &[
                    "-C",
                    "fix29-ssh-fetch",
                    "fetch",
                    &scoped_ssh,
                    "refs/libra/traces:refs/remotes/h1/traces",
                ],
            );
            git_cli::assert_git_success(&fetched, "FIX29 real scoped SSH fetch");
            assert_eq!(
                f.fixture.git_text(&[
                    "-C",
                    "fix29-ssh-fetch",
                    "rev-parse",
                    "refs/remotes/h1/traces"
                ]),
                tip
            );
            assert_eq!(
                f.fixture.git(&[
                    "-C",
                    "fix29-ssh-fetch",
                    "show",
                    "refs/remotes/h1/traces:payload.bin"
                ]),
                b"FIX29 synthetic key scope\0\xff\n"
            );
            f.fixture
                .git(&["init", "--object-format=sha1", "fix29-http-fetch"]);
            let scoped_http = git_cli::mega2_http_url(f.http_port, &scope);
            let fetched = git_cli::git_cli_as_user(
                shared,
                SCOPE_WRITER,
                &token,
                &[
                    "-C",
                    "fix29-http-fetch",
                    "fetch",
                    &scoped_http,
                    "refs/libra/traces:refs/remotes/h1/traces",
                ],
            );
            git_cli::assert_git_success(&fetched, "FIX29 actual HTTP fetch of SSH-pushed scope");
            assert_eq!(
                f.fixture.git_text(&[
                    "-C",
                    "fix29-http-fetch",
                    "rev-parse",
                    "refs/remotes/h1/traces"
                ]),
                tip
            );
            assert_eq!(
                f.fixture.git(&[
                    "-C",
                    "fix29-http-fetch",
                    "show",
                    "refs/remotes/h1/traces:payload.bin"
                ]),
                b"FIX29 synthetic key scope\0\xff\n"
            );
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
            let logs = fixture.fixture.temp.keep();
            for name in ["process.out", "process.err"] {
                if let Ok(bytes) = fs::read(logs.join(name)) {
                    eprintln!(
                        "FIX29 fixture failure log: file={} sha256={}",
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
        closed.expect("close FIX29 pool");
    }

    pub async fn run_ssh_scoped_token_key_matrix() {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(ScopeAuthFixture::new(&db).await);
            let f = fixture.as_mut().expect("FIX29 matrix fixture");
            let target_b = Uuid::new_v4().to_string();
            trace_target::ActiveModel {
                id: Set(target_b), target_kind: Set("native".into()), project_path: Set("/.traces-backup".into()),
                backing_import_repo_id: Set(None), active: Set(true), created_at: Set(Utc::now().fixed_offset()),
            }.insert(&db).await.expect("second target");
            let a = f.key(SCOPE_WRITER).await;
            let b = f.key(SCOPE_WRITER).await;
            let mismatch = f.key(SCOPE_WRITER).await;
            let unbound = f.key(SCOPE_WRITER).await;
            let a_name = format!("trace-ssh-key:{}", a.id);
            let b_name = format!("trace-ssh-key:{}", b.id);
            let mismatch_name = format!("trace-ssh-key:{}", mismatch.id);
            let unbound_name = format!("trace-ssh-key:{}", unbound.id);
            let a_token = f.static_token(&a_name, "/");
            let b_token = f.static_token(&b_name, "/project");
            let mismatch_token = f.static_token(&mismatch_name, "/project");
            let _unbound_token = f.static_token(&unbound_name, "/project");
            f.bind(&f.owner, "mono_account", &format!("ssh-key:{}", a.id)).await;
            f.bind(&f.owner, "static_push_token", &a_name).await;
            f.bind(&f.outsider, "mono_account", &format!("ssh-key:{}", b.id)).await;
            f.bind(&f.outsider, "static_push_token", &b_name).await;
            f.bind(&f.outsider, "mono_account", &format!("ssh-key:{}", mismatch.id)).await;
            f.bind(&f.owner, "static_push_token", &mismatch_name).await;
            f.bind(&f.owner, "mono_account", &format!("ssh-key:{}", unbound.id)).await;
            f.start(true, true, "off").await;
            let shared = f.fixture.shared.path();
            let ssh_dir = shared.join("ssh");
            fs::create_dir_all(&ssh_dir).expect("FIX29 matrix ssh dir");
            git_cli::write_known_hosts_via_keyscan(&ssh_dir.join("known_hosts"), f.fixture.port);
            let ssh = git_cli::git_ssh_command(shared, f.fixture.port);
            f.fixture.git(&["init", "--object-format=sha1", "fix29-matrix-source"]);
            fs::write(shared.join("fix29-matrix-source/sentinel"), b"FIX29 matrix synthetic\0\xff\n").unwrap();
            f.fixture.git(&["-C", "fix29-matrix-source", "add", "sentinel"]);
            f.fixture.git(&["-C", "fix29-matrix-source", "-c", "user.name=FIX29", "-c", "user.email=fix29@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "matrix"]);
            let tip = f.fixture.git_text(&["-C", "fix29-matrix-source", "rev-parse", "HEAD"]);
            let mut scopes = Vec::new();
            for (key, target, source, owner) in [(&a, "/project", "fix29-a-one", &f.owner), (&a, "/.traces-backup", "fix29-a-two", &f.owner), (&b, "/project", "fix29-b-one", &f.outsider)] {
                fs::copy(&key.path, ssh_dir.join("client_ed25519")).expect("select actual SSH key");
                let locator = wire_by_path(target, source, &f.fixture.epoch, "sha1");
                let url = format!("ssh://traces@{}:{}{}", git_cli::mega2_reachable_host(), f.fixture.port, locator);
                let output = git_cli::git_cli_ssh(shared, &ssh, &["-C", "fix29-matrix-source", "push", "--no-thin", &url, "HEAD:refs/libra/traces"]);
                git_cli::assert_git_success(&output, "FIX29 matrix actual SSH push");
                let stream = trace_stream::Entity::find().filter(trace_stream::Column::PrincipalId.eq(owner.clone())).filter(trace_stream::Column::SourceRepoId.eq(source)).one(&db).await.unwrap().expect("matrix stream");
                assert_eq!(stream.tip_oid.as_deref(), Some(tip.as_str()));
                scopes.push(format!("/.traces/by-scope/{}.git", stream.id));
            }
            // B can read A's T1, but neither read nor write A's T2.
            let b_session = f.key_session("traces", &b, true).await;
            let a_scope = scopes[0].clone();
            let a_scope_id = a_scope.trim_start_matches("/.traces/by-scope/").trim_end_matches(".git");
            wire_expect_metadata(f, &b_token, Some(&b_session), WireExpected { locator: &a_scope, kind: "sha1", scope: Some(a_scope_id), v2: true, receive: false }).await;
            for receive in [false, true] {
                wire_expect_refusal(f, Some(&b_token), Some(&b_session), &scopes[1], false, receive, 404).await;
            }
            fs::copy(&b.path, ssh_dir.join("client_ed25519")).expect("peer key");
            f.fixture.git(&["init", "--object-format=sha1", "fix29-peer-fetch"]);
            let url = format!("ssh://traces@{}:{}{}", git_cli::mega2_reachable_host(), f.fixture.port, scopes[0]);
            let fetch = git_cli::git_cli_ssh(shared, &ssh, &["-C", "fix29-peer-fetch", "fetch", &url, "refs/libra/traces:refs/remotes/h1/traces"]);
            git_cli::assert_git_success(&fetch, "FIX29 peer actual SSH read");
            assert_eq!(f.fixture.git(&["-C", "fix29-peer-fetch", "show", "refs/remotes/h1/traces:sentinel"]), b"FIX29 matrix synthetic\0\xff\n");
            // A valid password for the other principal is a positive control;
            // the mismatched key cannot inherit that token's rights.
            let wrong = f.key_session("traces", &mismatch, true).await;
            let status = wire_http_request(f, Some(&mismatch_token), "fix29-mismatch-positive", &scopes[0], false, false, None).await;
            assert_eq!(status.status, 200);
            ssh_refused(&wrong, &format!("git-upload-pack '{}'", scopes[0]), false).await;
            close_scope_session(wrong).await;
            let missing = f.key_session("traces", &unbound, true).await;
            ssh_refused(&missing, &format!("git-upload-pack '{}'", scopes[0]), false).await;
            close_scope_session(missing).await;
            // Revoke static binding while a verified SSH session stays open.
            db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "UPDATE trace_credential_binding SET retired_at=now() WHERE namespace='static_push_token' AND credential_id=$1 AND retired_at IS NULL", [b_name.clone().into()])).await.unwrap();
            ssh_refused(&b_session, &format!("git-upload-pack '{}'", scopes[0]), false).await;
            f.bind(&f.outsider, "static_push_token", &b_name).await;
            db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "UPDATE trace_credential_binding SET retired_at=now() WHERE namespace='mono_account' AND credential_id=$1 AND retired_at IS NULL", [format!("ssh-key:{}", b.id).into()])).await.unwrap();
            let status = wire_http_request(f, Some(&b_token), "fix29-key-revoked-http-positive", &scopes[0], false, false, None).await;
            assert_eq!(status.status, 200);
            ssh_refused(&b_session, &format!("git-upload-pack '{}'", scopes[0]), false).await;
            close_scope_session(b_session).await;
            let a_session = f.key_session("traces", &a, true).await;
            let scope_id = scopes[1].trim_start_matches("/.traces/by-scope/").trim_end_matches(".git");
            wire_expect_metadata(f, &a_token, Some(&a_session), WireExpected { locator: &scopes[1], kind: "sha1", scope: Some(scope_id), v2: true, receive: false }).await;
            close_scope_session(a_session).await;
        }).catch_unwind().await;
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
            let logs = fixture.fixture.temp.keep();
            for name in ["process.out", "process.err"] {
                if let Ok(bytes) = fs::read(logs.join(name)) {
                    eprintln!(
                        "FIX29 matrix log: file={} sha256={}",
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
        closed.expect("close FIX29 matrix pool");
    }

    pub async fn ssh_git_roundtrip(kind: &str) {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(ScopeAuthFixture::new_with_kind(&db, kind).await);
            let f = fixture.as_mut().expect("SSH Git fixture");
            let writer = f.key(SCOPE_WRITER).await;
            f.bind(&f.owner, "mono_account", &format!("ssh-key:{}", writer.id))
                .await;
            let outsider = f.key(SCOPE_WRITER).await;
            f.bind(
                &f.outsider,
                "mono_account",
                &format!("ssh-key:{}", outsider.id),
            )
            .await;
            let token = format!("mtr12-http-{}", Uuid::new_v4());
            mega2_core::access_token::ActiveModel {
                id: Set(1001),
                username: Set(SCOPE_WRITER.to_owned()),
                token: Set(token.clone()),
                created_at: Set(Utc::now().naive_utc()),
            }
            .insert(&db)
            .await
            .expect("seed same-principal HTTP token");
            f.bind(&f.owner, "mono_account", "access-token:1001").await;
            f.start(false, false, "enforce").await;
            let locator = wire_by_path("/project", "mtr12-new-source", &f.fixture.epoch, kind);
            let session = f.key_session("traces", &writer, true).await;
            let expected = WireExpected {
                locator: &locator,
                kind,
                scope: None,
                v2: false,
                receive: true,
            };
            let advertisement = wire_ssh_advertisement(&session, &locator, false, true).await;
            wire_metadata(&advertisement, false, &expected);
            close_scope_session(session).await;
            let shared = f.fixture.shared.path();
            let ssh_dir = shared.join("ssh");
            fs::create_dir_all(&ssh_dir).expect("SSH fixture directory");
            fs::copy(&writer.path, ssh_dir.join("client_ed25519")).expect("owned Git key");
            git_cli::write_known_hosts_via_keyscan(&ssh_dir.join("known_hosts"), f.fixture.port);
            let ssh = git_cli::git_ssh_command(shared, f.fixture.port);
            let url = format!(
                "ssh://traces@{}:{}{}",
                git_cli::mega2_reachable_host(),
                f.fixture.port,
                locator
            );
            f.fixture
                .git(&["init", &format!("--object-format={kind}"), "mtr12-source"]);
            fs::write(
                shared.join("mtr12-source/payload.bin"),
                b"MTR12 actual SSH trace payload\0\xff\n",
            )
            .expect("synthetic blob");
            f.fixture.git(&["-C", "mtr12-source", "add", "payload.bin"]);
            f.fixture.git(&[
                "-C",
                "mtr12-source",
                "-c",
                "user.name=MTR12",
                "-c",
                "user.email=mtr12@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "SSH traces",
            ]);
            let tip = f
                .fixture
                .git_text(&["-C", "mtr12-source", "rev-parse", "HEAD"]);
            let blob = f
                .fixture
                .git_text(&["-C", "mtr12-source", "rev-parse", "HEAD:payload.bin"]);
            let push = git_cli::git_cli_ssh(
                shared,
                &ssh,
                &[
                    "-C",
                    "mtr12-source",
                    "push",
                    "--no-thin",
                    &url,
                    "HEAD:refs/libra/traces",
                ],
            );
            assert!(
                push.status.success(),
                "MTR12_SSH_PACK_ORACLE: authenticated standard Git first push failed: {}",
                String::from_utf8_lossy(&push.stderr)
            );
            let stream = trace_stream::Entity::find()
                .filter(trace_stream::Column::PrincipalId.eq(f.owner.clone()))
                .filter(trace_stream::Column::SourceRepoId.eq("mtr12-new-source"))
                .one(&db)
                .await
                .expect("query pushed stream")
                .expect("first push created scope");
            assert_eq!(stream.tip_oid.as_deref(), Some(tip.as_str()));
            assert_eq!(stream.hash_kind, kind);
            let scope_path = format!("/.traces/by-scope/{}.git", stream.id);
            let http_scope = git_cli::mega2_http_url(f.http_port, &scope_path);
            let session = f.key_session("traces", &writer, true).await;
            let expected = WireExpected {
                locator: &scope_path,
                kind,
                scope: Some(&stream.id),
                v2: false,
                receive: false,
            };
            let before_discovery = snapshot(&db).await;
            let http_advertisement = wire_http_request(
                f,
                Some(&token),
                "mtr12-after-first-push",
                &scope_path,
                false,
                false,
                None,
            )
            .await;
            assert_eq!(
                http_advertisement.status, 200,
                "bound same-principal HTTP discovery"
            );
            let http_metadata =
                wire_metadata_with_tip(&http_advertisement.body, true, &expected, Some(&tip));
            let ssh_advertisement =
                wire_ssh_advertisement(&session, &scope_path, false, false).await;
            assert_eq!(
                wire_metadata_with_tip(&ssh_advertisement, false, &expected, Some(&tip)),
                http_metadata,
                "same server, scope, format, and tip over HTTP and SSH"
            );
            unchanged(&before_discovery, &snapshot(&db).await);
            close_scope_session(session).await;
            f.fixture.git(&[
                "init",
                &format!("--object-format={kind}"),
                "mtr12-http-download",
            ]);
            let http_fetch = git_cli::git_cli_as_user(
                shared,
                SCOPE_WRITER,
                &token,
                &[
                    "-C",
                    "mtr12-http-download",
                    "fetch",
                    &http_scope,
                    "refs/libra/traces:refs/remotes/origin/traces",
                ],
            );
            git_cli::assert_git_success(&http_fetch, "MTR12 SSH-to-HTTP fetch");
            assert_eq!(
                f.fixture.git_text(&[
                    "-C",
                    "mtr12-http-download",
                    "rev-parse",
                    "refs/remotes/origin/traces"
                ]),
                tip
            );
            fs::write(
                shared.join("mtr12-source/second.txt"),
                b"MTR12 HTTP append\n",
            )
            .expect("second synthetic blob");
            f.fixture.git(&["-C", "mtr12-source", "add", "second.txt"]);
            f.fixture.git(&[
                "-C",
                "mtr12-source",
                "-c",
                "user.name=MTR12",
                "-c",
                "user.email=mtr12@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "HTTP append",
            ]);
            let second_tip = f
                .fixture
                .git_text(&["-C", "mtr12-source", "rev-parse", "HEAD"]);
            let http_push = git_cli::git_cli_as_user(
                shared,
                SCOPE_WRITER,
                &token,
                &[
                    "-C",
                    "mtr12-source",
                    "push",
                    "--no-thin",
                    &http_scope,
                    "HEAD:refs/libra/traces",
                ],
            );
            git_cli::assert_git_success(&http_push, "MTR12 HTTP-to-SSH append");
            let current = trace_stream::Entity::find_by_id(&stream.id)
                .one(&db)
                .await
                .expect("query appended scope")
                .expect("scope retained");
            assert_eq!(current.tip_oid.as_deref(), Some(second_tip.as_str()));
            let scoped = format!(
                "ssh://traces@{}:{}/.traces/by-scope/{}.git",
                git_cli::mega2_reachable_host(),
                f.fixture.port,
                stream.id
            );
            f.fixture
                .git(&["init", &format!("--object-format={kind}"), "mtr12-download"]);
            let fetch = git_cli::git_cli_ssh(
                shared,
                &ssh,
                &[
                    "-C",
                    "mtr12-download",
                    "-c",
                    "protocol.version=0",
                    "fetch",
                    &scoped,
                    "refs/libra/traces:refs/remotes/origin/traces",
                ],
            );
            git_cli::assert_git_success(&fetch, "MTR12 SSH fetch");
            assert_eq!(
                f.fixture.git_text(&[
                    "-C",
                    "mtr12-download",
                    "rev-parse",
                    "refs/remotes/origin/traces"
                ]),
                second_tip
            );
            f.fixture.git(&[
                "init",
                &format!("--object-format={kind}"),
                "mtr12-download-v2",
            ]);
            let fetch_v2 = git_cli::git_cli_ssh(
                shared,
                &ssh,
                &[
                    "-C",
                    "mtr12-download-v2",
                    "-c",
                    "protocol.version=2",
                    "fetch",
                    &scoped,
                    "refs/libra/traces:refs/remotes/origin/traces",
                ],
            );
            git_cli::assert_git_success(&fetch_v2, "MTR12 SSH v2 fetch");
            assert_eq!(
                f.fixture.git_text(&[
                    "-C",
                    "mtr12-download-v2",
                    "rev-parse",
                    "refs/remotes/origin/traces"
                ]),
                second_tip
            );
            assert_eq!(
                f.fixture
                    .git(&["-C", "mtr12-download", "cat-file", "blob", &blob]),
                b"MTR12 actual SSH trace payload\0\xff\n"
            );
            f.fixture.git(&["-C", "mtr12-download", "fsck", "--strict"]);
            if kind == "sha1" {
                // Deterministic incompressible payload forces a real PACK larger
                // than the russh 100-message input queue, without a network mock.
                let mut state = 0x8ad4_b12c_79e3_5f01_u64;
                let mut bytes = vec![0_u8; 5 * 1024 * 1024];
                for byte in &mut bytes {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = state as u8;
                }
                fs::write(shared.join("mtr12-source/large.bin"), &bytes)
                    .expect("large synthetic blob");
                f.fixture.git(&["-C", "mtr12-source", "add", "large.bin"]);
                f.fixture.git(&[
                    "-C",
                    "mtr12-source",
                    "-c",
                    "user.name=MTR12",
                    "-c",
                    "user.email=mtr12@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "large SSH append",
                ]);
                let large_tip = f
                    .fixture
                    .git_text(&["-C", "mtr12-source", "rev-parse", "HEAD"]);
                let large_blob =
                    f.fixture
                        .git_text(&["-C", "mtr12-source", "rev-parse", "HEAD:large.bin"]);
                let pack =
                    f.fixture
                        .git(&["-C", "mtr12-source", "pack-objects", "--stdout", "--all"]);
                assert!(
                    pack.len() > 4 * 1024 * 1024,
                    "actual standard Git PACK must exceed 4 MiB"
                );
                drop(pack);
                let push = git_cli::git_cli_ssh(
                    shared,
                    &ssh,
                    &[
                        "-C",
                        "mtr12-source",
                        "push",
                        "--no-thin",
                        &scoped,
                        "HEAD:refs/libra/traces",
                    ],
                );
                git_cli::assert_git_success(&push, "MTR12 large SSH append");
                let http_fetch = git_cli::git_cli_as_user(
                    shared,
                    SCOPE_WRITER,
                    &token,
                    &[
                        "-C",
                        "mtr12-http-download",
                        "fetch",
                        &http_scope,
                        "refs/libra/traces:refs/remotes/origin/traces",
                    ],
                );
                git_cli::assert_git_success(&http_fetch, "MTR12 large SSH-to-HTTP fetch");
                assert_eq!(
                    f.fixture.git_text(&[
                        "-C",
                        "mtr12-http-download",
                        "rev-parse",
                        "refs/remotes/origin/traces"
                    ]),
                    large_tip
                );
                assert_eq!(
                    f.fixture
                        .git(&["-C", "mtr12-http-download", "cat-file", "blob", &large_blob]),
                    bytes
                );
                f.fixture
                    .git(&["-C", "mtr12-http-download", "fsck", "--strict"]);

                // Keep an authenticated SSH receive open after fragmented
                // commands and a partial PACK, then prove shared admission,
                // independent fetch and recovery after peer disconnect.
                let session = f.key_session("traces", &writer, true).await;
                let mut channel = session
                    .channel_open_session()
                    .await
                    .expect("held receive channel");
                channel
                    .exec(true, format!("git-receive-pack '{scope_path}'"))
                    .await
                    .expect("held receive exec");
                let advertisement = tokio::time::timeout(Duration::from_secs(10), async {
                    let mut bytes = Vec::new();
                    let mut accepted = false;
                    while let Some(message) = channel.wait().await {
                        match message {
                            russh::ChannelMsg::Success => accepted = true,
                            russh::ChannelMsg::Data { data } => bytes.extend_from_slice(&data),
                            russh::ChannelMsg::Failure | russh::ChannelMsg::Close => {
                                panic!("held receive refused")
                            }
                            _ => {}
                        }
                        if accepted && bytes.ends_with(b"0000") {
                            return bytes;
                        }
                    }
                    panic!("held receive closed before advertisement")
                })
                .await
                .expect("held receive advertisement deadline");
                let expected = WireExpected {
                    locator: &scope_path,
                    kind,
                    scope: Some(&stream.id),
                    v2: false,
                    receive: true,
                };
                wire_metadata_with_tip(&advertisement, false, &expected, Some(&large_tip));
                let command = format!(
                    "{} {} refs/libra/traces\0report-status object-format=sha1\n",
                    large_tip,
                    "1".repeat(40)
                );
                let header = format!("{:04x}", command.len() + 4);
                channel
                    .data_bytes(bytes::Bytes::copy_from_slice(&header.as_bytes()[..2]))
                    .await
                    .expect("fragment one");
                channel
                    .data_bytes(bytes::Bytes::copy_from_slice(&header.as_bytes()[2..]))
                    .await
                    .expect("fragment two");
                channel
                    .data_bytes(bytes::Bytes::copy_from_slice(command.as_bytes()))
                    .await
                    .expect("receive command");
                channel
                    .data_bytes(bytes::Bytes::from_static(b"0000PACK\0\0\0\x02\0\0\0\x01"))
                    .await
                    .expect("partial PACK");
                let busy = wire_http_request(
                    f,
                    Some(&token),
                    "mtr12-held-http",
                    &scope_path,
                    false,
                    true,
                    Some(b"0000"),
                )
                .await;
                assert_eq!(
                    busy.status, 503,
                    "HTTP and SSH must share the one receive slot"
                );
                f.fixture
                    .git(&["init", "--object-format=sha1", "mtr12-fetch-during-receive"]);
                let fetch = git_cli::git_cli_ssh(
                    shared,
                    &ssh,
                    &[
                        "-C",
                        "mtr12-fetch-during-receive",
                        "fetch",
                        &scoped,
                        "refs/libra/traces:refs/remotes/origin/traces",
                    ],
                );
                git_cli::assert_git_success(&fetch, "independent fetch while SSH receive is held");
                assert_eq!(
                    f.fixture.git_text(&[
                        "-C",
                        "mtr12-fetch-during-receive",
                        "rev-parse",
                        "refs/remotes/origin/traces"
                    ]),
                    large_tip
                );
                channel.close().await.expect("disconnect held receive");
                close_scope_session(session).await;
                let status = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let label = format!("mtr12-after-close-{}", Uuid::new_v4());
                        let result = wire_http_request(
                            f,
                            Some(&token),
                            &label,
                            &scope_path,
                            false,
                            true,
                            Some(b"0000"),
                        )
                        .await;
                        if result.status != 503 {
                            break result.status;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .expect("receive slot released after disconnect");
                assert_eq!(
                    status, 200,
                    "authorized empty receive must complete after slot release"
                );
                let current = trace_stream::Entity::find_by_id(&stream.id)
                    .one(&db)
                    .await
                    .expect("scope after disconnect")
                    .expect("scope retained");
                assert_eq!(
                    current.tip_oid.as_deref(),
                    Some(large_tip.as_str()),
                    "partial PACK must not publish"
                );
                fs::write(
                    shared.join("mtr12-source/after-disconnect.txt"),
                    b"post-disconnect\n",
                )
                .expect("recovery blob");
                f.fixture
                    .git(&["-C", "mtr12-source", "add", "after-disconnect.txt"]);
                f.fixture.git(&[
                    "-C",
                    "mtr12-source",
                    "-c",
                    "user.name=MTR12",
                    "-c",
                    "user.email=mtr12@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    "recovery after disconnect",
                ]);
                let recovery_tip = f
                    .fixture
                    .git_text(&["-C", "mtr12-source", "rev-parse", "HEAD"]);
                let recovery = git_cli::git_cli_ssh(
                    shared,
                    &ssh,
                    &[
                        "-C",
                        "mtr12-source",
                        "push",
                        "--no-thin",
                        &scoped,
                        "HEAD:refs/libra/traces",
                    ],
                );
                git_cli::assert_git_success(&recovery, "actual SSH push after disconnect");
                let current = trace_stream::Entity::find_by_id(&stream.id)
                    .one(&db)
                    .await
                    .expect("scope after recovery")
                    .expect("scope retained");
                assert_eq!(current.tip_oid.as_deref(), Some(recovery_tip.as_str()));

                // Interrupt a real multi-megabyte upload response after PACK
                // starts, then prove the fetch slot is released by another
                // authenticated Git fetch and exact object verification.
                let output_session = f.key_session("traces", &writer, true).await;
                let mut output_channel = output_session
                    .channel_open_session()
                    .await
                    .expect("output-close channel");
                output_channel
                    .exec(true, format!("git-upload-pack '{scope_path}'"))
                    .await
                    .expect("output-close exec");
                let output_adv = tokio::time::timeout(Duration::from_secs(10), async {
                    let mut bytes = Vec::new();
                    let mut accepted = false;
                    while let Some(message) = output_channel.wait().await {
                        match message {
                            russh::ChannelMsg::Success => accepted = true,
                            russh::ChannelMsg::Data { data } => bytes.extend_from_slice(&data),
                            russh::ChannelMsg::Failure | russh::ChannelMsg::Close => {
                                panic!("output-close discovery refused")
                            }
                            _ => {}
                        }
                        if accepted && bytes.ends_with(b"0000") {
                            return bytes;
                        }
                    }
                    panic!("output-close discovery ended early")
                })
                .await
                .expect("output-close discovery deadline");
                let expected = WireExpected {
                    locator: &scope_path,
                    kind,
                    scope: Some(&stream.id),
                    v2: false,
                    receive: false,
                };
                wire_metadata_with_tip(&output_adv, false, &expected, Some(&recovery_tip));
                let want = format!("want {recovery_tip}\n");
                let request = format!("{:04x}{}00000009done\n", want.len() + 4, want);
                output_channel
                    .data_bytes(bytes::Bytes::from(request))
                    .await
                    .expect("send upload request");
                tokio::time::timeout(LIMIT, async {
                    let mut first = Vec::new();
                    while let Some(message) = output_channel.wait().await {
                        match message {
                            russh::ChannelMsg::Data { data } => {
                                first.extend_from_slice(&data);
                                if first.windows(4).any(|window| window == b"PACK") {
                                    return;
                                }
                            }
                            russh::ChannelMsg::Failure | russh::ChannelMsg::Close => {
                                panic!("output-close before PACK")
                            }
                            _ => {}
                        }
                    }
                    panic!("output-close ended before PACK")
                })
                .await
                .expect("output PACK start deadline");
                output_channel
                    .close()
                    .await
                    .expect("disconnect during PACK output");
                close_scope_session(output_session).await;
                f.fixture
                    .git(&["init", "--object-format=sha1", "mtr12-after-output-close"]);
                let recovery_fetch = git_cli::git_cli_ssh(
                    shared,
                    &ssh,
                    &[
                        "-C",
                        "mtr12-after-output-close",
                        "fetch",
                        &scoped,
                        "refs/libra/traces:refs/remotes/origin/traces",
                    ],
                );
                git_cli::assert_git_success(
                    &recovery_fetch,
                    "actual fetch after output disconnect",
                );
                assert_eq!(
                    f.fixture.git_text(&[
                        "-C",
                        "mtr12-after-output-close",
                        "rev-parse",
                        "refs/remotes/origin/traces"
                    ]),
                    recovery_tip
                );
                assert_eq!(
                    f.fixture.git(&[
                        "-C",
                        "mtr12-after-output-close",
                        "cat-file",
                        "blob",
                        &large_blob
                    ]),
                    bytes
                );
                let object_count =
                    scalar(&db, "SELECT count(*)::text AS value FROM trace_object").await;
                let journal_count =
                    scalar(&db, "SELECT count(*)::text AS value FROM trace_ref_update").await;
                fs::copy(&outsider.path, ssh_dir.join("client_ed25519"))
                    .expect("switch to bound outsider key");
                let read = git_cli::git_cli_ssh(shared, &ssh, &["ls-remote", &scoped]);
                git_cli::assert_git_success(&read, "authorized nonowner read");
                assert!(
                    String::from_utf8_lossy(&read.stdout).contains(&recovery_tip),
                    "nonowner sees authorized current tip"
                );
                let denied = git_cli::git_cli_ssh(
                    shared,
                    &ssh,
                    &[
                        "-C",
                        "mtr12-source",
                        "push",
                        &scoped,
                        "HEAD:refs/libra/traces",
                    ],
                );
                assert!(
                    !denied.status.success(),
                    "nonowner must not update owner's stream"
                );
                fs::copy(&writer.path, ssh_dir.join("client_ed25519"))
                    .expect("restore owned writer key");
                let extra_ref = git_cli::git_cli_ssh(
                    shared,
                    &ssh,
                    &[
                        "-C",
                        "mtr12-source",
                        "push",
                        &scoped,
                        "HEAD:refs/heads/other",
                    ],
                );
                assert!(!extra_ref.status.success(), "extra ref must be rejected");
                let delete = git_cli::git_cli_ssh(
                    shared,
                    &ssh,
                    &["-C", "mtr12-source", "push", &scoped, ":refs/libra/traces"],
                );
                assert!(
                    !delete.status.success(),
                    "trace ref deletion must be rejected"
                );
                let rewrite = git_cli::git_cli_ssh(
                    shared,
                    &ssh,
                    &[
                        "-C",
                        "mtr12-source",
                        "push",
                        "--force",
                        "--no-thin",
                        &scoped,
                        &format!("{tip}:refs/libra/traces"),
                    ],
                );
                assert!(
                    !rewrite.status.success(),
                    "non-fast-forward trace update must be rejected"
                );
                let current = trace_stream::Entity::find_by_id(&stream.id)
                    .one(&db)
                    .await
                    .expect("scope after denied writes")
                    .expect("scope retained");
                assert_eq!(current.tip_oid.as_deref(), Some(recovery_tip.as_str()));
                assert_eq!(
                    scalar(&db, "SELECT count(*)::text AS value FROM trace_object").await,
                    object_count,
                    "denied writes must not alter membership"
                );
                assert_eq!(
                    scalar(&db, "SELECT count(*)::text AS value FROM trace_ref_update").await,
                    journal_count,
                    "denied writes must not alter journal"
                );
            }
        })
        .catch_unwind()
        .await;
        if let Some(f) = fixture.as_mut() {
            f.stop();
        }
        let closed = db.close().await;
        drop(guard);
        if let Err(panic) = result {
            if let Some(f) = fixture {
                let logs = f.fixture.temp.keep();
                eprintln!("MTR12 SSH fixture logs retained at {}", logs.display());
            }
            std::panic::resume_unwind(panic);
        }
        closed.expect("close SSH Git fixture pool");
    }

    pub async fn run_metadata_wire_matrix(kind: &str, with_ssh: bool) {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(ScopeAuthFixture::new_with_kind(&db, kind).await);
            let fixture = fixture.as_mut().expect("metadata wire fixture");
            assert_eq!(
                fixture.fixture.object_format, "sha1",
                "code format remains SHA1 even for SHA256 trace by-scope discovery"
            );
            let owner = fixture.owner.clone();
            let outsider = fixture.outsider.clone();
            let stream = trace_stream::Entity::find_by_id(&fixture.fixture.scope)
                .one(&db)
                .await
                .expect("read actual wire scope")
                .expect("wire scope exists");
            assert_eq!(
                stream.hash_kind, kind,
                "scope kind is immutable from its initial insert"
            );
            let target_a = trace_target::Entity::find_by_id(&stream.target_id)
                .one(&db)
                .await
                .expect("read target A")
                .expect("target A exists");
            assert_eq!(target_a.project_path, "/project");
            // Both paths are actual native root trees made by service init;
            // their credential prefixes remain disjoint.
            let target_b = Uuid::new_v4().to_string();
            let scope_b = Uuid::new_v4().to_string();
            trace_target::ActiveModel {
                id: Set(target_b.clone()),
                target_kind: Set("native".to_owned()),
                project_path: Set("/.traces-backup".to_owned()),
                backing_import_repo_id: Set(None),
                active: Set(true),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(&db)
            .await
            .expect("distinct real target B");
            trace_stream::ActiveModel {
                id: Set(scope_b.clone()),
                principal_id: Set(owner.clone()),
                target_id: Set(target_b),
                source_repo_id: Set("mtr39-auth-only".to_owned()),
                epoch_id: Set(fixture.fixture.epoch.clone()),
                hash_kind: Set(kind.to_owned()),
                tip_oid: Set(None),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(&db)
            .await
            .expect("distinct same-tuple target B stream");
            let own_scope = format!("/.traces/by-scope/{}.git", fixture.fixture.scope);
            let other_scope = format!("/.traces/by-scope/{scope_b}.git");
            let unknown = format!("/.traces/by-scope/{}.git", Uuid::new_v4());
            let own_path =
                wire_by_path("/project", "mtr39-auth-only", &fixture.fixture.epoch, kind);
            let empty_path =
                wire_by_path("/project", "absent-source", &fixture.fixture.epoch, kind);
            let other_path = wire_by_path(
                "/.traces-backup",
                "mtr39-auth-only",
                &fixture.fixture.epoch,
                kind,
            );
            let other_empty = wire_by_path(
                "/.traces-backup",
                "absent-source",
                &fixture.fixture.epoch,
                kind,
            );
            let own = fixture.static_token("mtr92-owner-a", "/project");
            fixture
                .bind(&owner, "static_push_token", "mtr92-owner-a")
                .await;
            let other = fixture.static_token("mtr92-other-principal-a", "/project");
            fixture
                .bind(&outsider, "static_push_token", "mtr92-other-principal-a")
                .await;
            let target_b_token = fixture.static_token("mtr92-owner-b", "/.traces-backup");
            fixture
                .bind(&owner, "static_push_token", "mtr92-owner-b")
                .await;
            let unbound = fixture.static_token("mtr92-wire-unbound", "/project");
            fixture.start(true, true, "off").await;
            let owner_session = if with_ssh {
                Some(fixture.password_session(&own, true).await)
            } else {
                None
            };
            let other_session = if with_ssh {
                Some(fixture.password_session(&other, true).await)
            } else {
                None
            };
            let target_b_session = if with_ssh {
                Some(fixture.password_session(&target_b_token, true).await)
            } else {
                None
            };
            let unbound_session = if with_ssh {
                Some(fixture.password_session(&unbound, true).await)
            } else {
                None
            };
            let anonymous = if with_ssh {
                let mut session = fixture.connect().await;
                assert!(
                    tokio::time::timeout(Duration::from_secs(10), session.authenticate_none("git"))
                        .await
                        .expect("anonymous wire auth deadline")
                        .expect("anonymous wire auth")
                        .success()
                );
                Some(session)
            } else {
                None
            };
            for v2 in [false, true] {
                for (locator, scope) in [
                    (&own_scope, Some(fixture.fixture.scope.as_str())),
                    (&own_path, Some(fixture.fixture.scope.as_str())),
                    (&empty_path, None),
                ] {
                    wire_expect_metadata(
                        fixture,
                        &own,
                        owner_session.as_ref(),
                        WireExpected {
                            locator,
                            kind,
                            scope,
                            v2,
                            receive: false,
                        },
                    )
                    .await;
                }
                // Shared project read is allowed, while by-path self never aliases another principal.
                wire_expect_metadata(
                    fixture,
                    &other,
                    other_session.as_ref(),
                    WireExpected {
                        locator: &own_scope,
                        kind,
                        scope: Some(&fixture.fixture.scope),
                        v2,
                        receive: false,
                    },
                )
                .await;
                wire_expect_metadata(
                    fixture,
                    &other,
                    other_session.as_ref(),
                    WireExpected {
                        locator: &own_path,
                        kind,
                        scope: None,
                        v2,
                        receive: false,
                    },
                )
                .await;
                wire_expect_metadata(
                    fixture,
                    &target_b_token,
                    target_b_session.as_ref(),
                    WireExpected {
                        locator: &other_path,
                        kind,
                        scope: Some(&scope_b),
                        v2,
                        receive: false,
                    },
                )
                .await;
                let denied_existing = wire_expect_refusal(
                    fixture,
                    Some(&own),
                    owner_session.as_ref(),
                    &other_scope,
                    v2,
                    false,
                    404,
                )
                .await;
                let denied_unknown = wire_expect_refusal(
                    fixture,
                    Some(&own),
                    owner_session.as_ref(),
                    &unknown,
                    v2,
                    false,
                    404,
                )
                .await;
                assert_eq!(
                    denied_existing, denied_unknown,
                    "cross-target scope existence remains hidden"
                );
                let denied_existing = wire_expect_refusal(
                    fixture,
                    Some(&own),
                    owner_session.as_ref(),
                    &other_path,
                    v2,
                    false,
                    404,
                )
                .await;
                let denied_empty = wire_expect_refusal(
                    fixture,
                    Some(&own),
                    owner_session.as_ref(),
                    &other_empty,
                    v2,
                    false,
                    404,
                )
                .await;
                assert_eq!(
                    denied_existing, denied_empty,
                    "cross-target by-path existence remains hidden"
                );
                for locator in [&own_scope, &empty_path, &unknown] {
                    wire_expect_refusal(fixture, None, anonymous.as_ref(), locator, v2, false, 401)
                        .await;
                    wire_expect_refusal(
                        fixture,
                        Some(&unbound),
                        unbound_session.as_ref(),
                        locator,
                        v2,
                        false,
                        404,
                    )
                    .await;
                }
                wire_code_control(fixture, anonymous.as_ref(), v2).await;
            }
            for (locator, scope) in [
                (&own_scope, Some(fixture.fixture.scope.as_str())),
                (&own_path, Some(fixture.fixture.scope.as_str())),
                (&empty_path, None),
            ] {
                wire_expect_metadata(
                    fixture,
                    &own,
                    owner_session.as_ref(),
                    WireExpected {
                        locator,
                        kind,
                        scope,
                        v2: false,
                        receive: true,
                    },
                )
                .await;
            }
            let denied_existing = wire_expect_refusal(
                fixture,
                Some(&other),
                other_session.as_ref(),
                &own_scope,
                false,
                true,
                404,
            )
            .await;
            let denied_unknown = wire_expect_refusal(
                fixture,
                Some(&other),
                other_session.as_ref(),
                &unknown,
                false,
                true,
                404,
            )
            .await;
            assert_eq!(
                denied_existing, denied_unknown,
                "non-owner receive existence remains hidden"
            );
            wire_standard_push_status(fixture, &own, kind).await;
            if let Some(session) = owner_session {
                close_scope_session(session).await;
            }
            if let Some(session) = other_session {
                close_scope_session(session).await;
            }
            if let Some(session) = target_b_session {
                close_scope_session(session).await;
            }
            if let Some(session) = unbound_session {
                close_scope_session(session).await;
            }
            if let Some(session) = anonymous {
                close_scope_session(session).await;
            }
            fixture.start(true, true, "off").await;
            let session = if with_ssh {
                Some(fixture.password_session(&own, true).await)
            } else {
                None
            };
            wire_expect_metadata(
                fixture,
                &own,
                session.as_ref(),
                WireExpected {
                    locator: &own_scope,
                    kind,
                    scope: Some(&fixture.fixture.scope),
                    v2: true,
                    receive: false,
                },
            )
            .await;
            if let Some(session) = session {
                close_scope_session(session).await;
            }
            // A current Mono account/key is a separate credential source, with the real shared Cedar store.
            let mono = wire_access_token(fixture, SCOPE_WRITER, &owner).await;
            let nonwriter = wire_access_token(fixture, SCOPE_NONWRITER, &owner).await;
            let writer_key = if with_ssh {
                let key = fixture.key(SCOPE_WRITER).await;
                fixture
                    .bind(&owner, "mono_account", &format!("ssh-key:{}", key.id))
                    .await;
                Some(key)
            } else {
                None
            };
            let nonwriter_key = if with_ssh {
                let key = fixture.key(SCOPE_NONWRITER).await;
                fixture
                    .bind(&owner, "mono_account", &format!("ssh-key:{}", key.id))
                    .await;
                Some(key)
            } else {
                None
            };
            fixture.start(false, false, "enforce").await;
            let session = if let Some(key) = &writer_key {
                Some(fixture.key_session("traces", key, true).await)
            } else {
                None
            };
            let denied = if let Some(key) = &nonwriter_key {
                Some(fixture.key_session("traces", key, true).await)
            } else {
                None
            };
            for v2 in [false, true] {
                wire_expect_metadata(
                    fixture,
                    &mono,
                    session.as_ref(),
                    WireExpected {
                        locator: &own_scope,
                        kind,
                        scope: Some(&fixture.fixture.scope),
                        v2,
                        receive: false,
                    },
                )
                .await;
                let existing = wire_expect_refusal(
                    fixture,
                    Some(&nonwriter),
                    denied.as_ref(),
                    &own_scope,
                    v2,
                    false,
                    404,
                )
                .await;
                let missing = wire_expect_refusal(
                    fixture,
                    Some(&nonwriter),
                    denied.as_ref(),
                    &unknown,
                    v2,
                    false,
                    404,
                )
                .await;
                assert_eq!(
                    existing, missing,
                    "current owner credential without project read cannot discover existence"
                );
            }
            wire_expect_metadata(
                fixture,
                &mono,
                session.as_ref(),
                WireExpected {
                    locator: &own_scope,
                    kind,
                    scope: Some(&fixture.fixture.scope),
                    v2: false,
                    receive: true,
                },
            )
            .await;
            wire_expect_refusal(
                fixture,
                Some(&nonwriter),
                denied.as_ref(),
                &own_scope,
                false,
                true,
                404,
            )
            .await;
            if let Some(session) = session {
                close_scope_session(session).await;
            }
            if let Some(session) = denied {
                close_scope_session(session).await;
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
            let logs = fixture.fixture.temp.keep();
            for name in ["process.out", "process.err"] {
                if let Ok(bytes) = fs::read(logs.join(name)) {
                    eprintln!(
                        "metadata wire failure log: file={} sha256={}",
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
        closed.expect("close metadata wire pool before database guard");
    }

    #[derive(Clone, Copy)]
    pub enum ScopeAuthCase {
        Anonymous,
        Static,
        Mono,
        Deleted,
        Replaced,
        Ambiguous,
        VerifiedKey,
    }

    pub async fn scope_auth(case: ScopeAuthCase) {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(ScopeAuthFixture::new(&db).await);
            let f = fixture.as_mut().expect("scope auth fixture");
            let writer = f.key(SCOPE_WRITER).await;
            f.bind(&f.owner, "mono_account", &format!("ssh-key:{}", writer.id)).await;
            match case {
                ScopeAuthCase::Anonymous => {
                    f.start(true, false, "off").await;
                    let mut session = f.connect().await;
                    assert!(tokio::time::timeout(Duration::from_secs(10), session.authenticate_none("git")).await.expect("ordinary anonymous auth deadline").expect("ordinary anonymous auth").success());
                    f.receive_refused(&session).await;
                    let before = snapshot(&db).await;
                    let path = format!("/.traces/by-scope/{}.git", f.fixture.scope);
                    for service in ["git-upload-pack", "git-lfs-authenticate", "git-lfs-transfer"] {
                        let suffix = if service.starts_with("git-lfs") { " download" } else { "" };
                        ssh_refused(&session, &format!("{service} '{path}'{suffix}"), false).await;
                    }
                    unchanged(&before, &snapshot(&db).await);
                    f.code_receive_refused(&session).await;
                    close_scope_session(session).await;
                }
                ScopeAuthCase::Static => {
                    // Deliberate textual collision with the actual Mono row id.
                    let collision = format!("ssh-key:{}", writer.id);
                    let token = f.static_token(&collision, "/project");
                    f.bind(&f.outsider, "static_push_token", &collision).await;
                    let own_token = f.static_token("mtr39-owner", "/project");
                    f.bind(&f.owner, "static_push_token", "mtr39-owner").await;
                    let narrow = f.static_token("mtr39-uncovered", "/other-project");
                    f.bind(&f.owner, "static_push_token", "mtr39-uncovered").await;
                    let unbound = f.static_token("mtr39-unbound", "/project");
                    f.start(true, true, "off").await;
                    for password in [&token, &narrow, &unbound] {
                        let session = f.password_session(password, true).await;
                        f.receive_refused(&session).await;
                        close_scope_session(session).await;
                    }
                    let session = f.password_session(&own_token, true).await;
                    f.receive_allowed(&session).await;
                    f.code_receive_refused(&session).await;
                    close_scope_session(session).await;
                    close_scope_session(f.password_session("mtr39-invalid-password", false).await).await;
                }
                ScopeAuthCase::Mono => {
                    let nonwriter = f.key(SCOPE_NONWRITER).await;
                    f.bind(&f.owner, "mono_account", &format!("ssh-key:{}", nonwriter.id)).await;
                    let outsider = f.key(SCOPE_WRITER).await;
                    f.bind(&f.outsider, "mono_account", &format!("ssh-key:{}", outsider.id)).await;
                    let unbound = f.key(SCOPE_WRITER).await;
                    // A same-name static owner binding must not authorize this key.
                    f.bind(&f.owner, "static_push_token", &format!("ssh-key:{}", unbound.id)).await;
                    f.start(false, false, "enforce").await;
                    let session = f.key_session("traces", &writer, true).await;
                    f.receive_allowed(&session).await;
                    f.code_receive_refused(&session).await;
                    close_scope_session(session).await;
                    for key in [&nonwriter, &outsider, &unbound] {
                        let session = f.key_session("traces", key, true).await;
                        f.receive_refused(&session).await;
                        close_scope_session(session).await;
                    }
                    // Login text cannot impersonate a bound trace principal.
                    let session = f.key_session(SCOPE_WRITER, &unbound, true).await;
                    f.receive_refused(&session).await;
                    close_scope_session(session).await;
                    f.start(false, false, "off").await;
                    let session = f.key_session("traces", &writer, true).await;
                    f.receive_refused(&session).await;
                    close_scope_session(session).await;
                    // Traces shadow follows the real Cedar decision for this account.
                    f.start(false, false, "shadow").await;
                    let session = f.key_session("traces", &writer, true).await;
                    f.receive_allowed(&session).await;
                    close_scope_session(session).await;
                    let session = f.key_session("traces", &nonwriter, true).await;
                    f.receive_refused(&session).await;
                    close_scope_session(session).await;
                }
                ScopeAuthCase::Deleted | ScopeAuthCase::Replaced => {
                    f.start(false, false, "enforce").await;
                    let session = f.key_session("traces", &writer, true).await;
                    f.receive_allowed(&session).await;
                    if matches!(case, ScopeAuthCase::Deleted) {
                        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres, "DELETE FROM ssh_keys WHERE id=$1", [writer.id.into()])).await.expect("delete authenticated source row");
                    } else {
                        let replacement = f.key(SCOPE_WRITER).await;
                        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres, "DELETE FROM ssh_keys WHERE id=$1", [replacement.id.into()])).await.expect("remove temporary replacement source id");
                        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres, "UPDATE ssh_keys SET ssh_key=$1, finger=$2, created_at=created_at + interval '1 second' WHERE id=$3", [replacement.public.into(), replacement.fingerprint.into(), writer.id.into()])).await.expect("replace same authenticated source id");
                    }
                    f.receive_refused(&session).await;
                    close_scope_session(session).await;
                    close_scope_session(f.key_session("traces", &writer, false).await).await;
                }
                ScopeAuthCase::Ambiguous => {
                    f.start(false, false, "enforce").await;
                    for username in [SCOPE_WRITER, SCOPE_NONWRITER] {
                        let live = f.key_session("traces", &writer, true).await;
                        f.next_key_id += 1;
                        let duplicate = f.next_key_id;
                        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                            "INSERT INTO ssh_keys (id,username,title,ssh_key,finger,created_at) VALUES ($1,$2,$3,$4,$5,now())",
                            [duplicate.into(), username.into(), "MTR39 ambiguous source".into(), writer.public.clone().into(), writer.fingerprint.clone().into()],
                        )).await.expect("duplicate real fingerprint source row");
                        f.receive_refused(&live).await;
                        close_scope_session(live).await;
                        close_scope_session(f.key_session("traces", &writer, false).await).await;
                        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres, "DELETE FROM ssh_keys WHERE id=$1", [duplicate.into()])).await.expect("remove own ambiguous source row");
                    }
                    let session = f.key_session("traces", &writer, true).await;
                    f.receive_allowed(&session).await;
                    close_scope_session(session).await;
                }
                ScopeAuthCase::VerifiedKey => {
                    let unbound = f.key(SCOPE_WRITER).await;
                    f.start(false, false, "enforce").await;
                    let mut session = f.connect().await;
                    let offered = russh::keys::load_secret_key(&writer.path, None).expect("offered bound public key");
                    let wrong = std::sync::Arc::new(russh::keys::load_secret_key(&unbound.path, None).expect("different signing key"));
                    let mut signer = WrongScopeSigner { wrong: wrong.clone(), called: false };
                    let response = tokio::time::timeout(Duration::from_secs(10), session.authenticate_publickey_with("traces", offered.public_key().clone(), None, &mut signer)).await.expect("wrong-signature response deadline").expect("real wrong-signature response");
                    assert!(signer.called, "known public key must reach actual signature request");
                    assert!(!response.success(), "offered key without its signature must not authenticate");
                    // This is still the same unauthenticated transport. A valid
                    // different key must not inherit the previously offered A.
                    assert!(tokio::time::timeout(Duration::from_secs(10), session.authenticate_publickey("traces", russh::keys::PrivateKeyWithHashAlg::new(wrong, None))).await.expect("valid second-key deadline").expect("valid second-key response").success());
                    f.receive_refused(&session).await;
                    close_scope_session(session).await;
                    let session = f.key_session("traces", &writer, true).await;
                    f.receive_allowed(&session).await;
                    close_scope_session(session).await;
                }
            }
        }).catch_unwind().await;
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
            let logs = fixture.fixture.temp.keep();
            for name in ["process.out", "process.err"] {
                if let Ok(bytes) = fs::read(logs.join(name)) {
                    eprintln!(
                        "SSH scope fixture failure log retained: file={} sha256={}",
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
        closed.expect("close scope auth pool before schema guard");
    }

    pub async fn reserved_login_rejects_none() {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = None;
        let result = AssertUnwindSafe(async {
            fixture = Some(Fixture::new(&db).await);
            let fixture = fixture.as_mut().expect("SSH none-auth fixture");
            let mut init = fixture.command(false, "trunk", "/third-party");
            init.args(["service", "init", "--yes"]);
            OwnedChild::spawn(&mut init).require_success().await;
            fixture.start_ssh(true, true).await;
            assert!(
                fixture.ssh_none_authenticated("git").await,
                "ordinary storage-only git login must retain anonymous authentication"
            );
            assert!(
                !fixture.ssh_none_authenticated("traces").await,
                "reserved SSH login traces accepted none authentication"
            );
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
                        "SSH none-auth fixture failure log retained: file={} sha256={}",
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
        closed.expect("close SSH none-auth pool before database guard");
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
        // The working tree lives on a host path bind-mounted into the Docker Git
        // container, and a host write can lag behind `git add`. Retry staging
        // until the index actually differs from HEAD, otherwise `commit` sees a
        // clean tree and exits 1 with empty stderr (a fixture race, not product
        // behaviour).
        let mut staged = false;
        for _ in 0..40 {
            fixture.git(&["-C", "trace-source", "add", "."]);
            let probe = git_cli::git_cli_no_auth(
                fixture.shared.path(),
                &["-C", "trace-source", "diff", "--cached", "--quiet"],
            );
            if !probe.status.success() {
                staged = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(staged, "HTTP traces fixture could not stage the mutation");
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

    /// Capacity, timeout, and same-barrier ref-race controls at the endpoint:
    /// 1. No-legacy >4,096 refs (4,099 refs) and >4,096 orphan `git_tag` rows
    ///    serve advertisement, `git clone`, and `git fetch` without 413.
    /// 2. Legacy view (`refs/libra/traces` present) with >4,096 refs answers 413.
    /// 3. Legacy view (`refs/libra/traces` present) with >4,096 tags answers 413.
    /// 4. Commit budget (>200,000 commits in `B(mode)`) answers 413 at upload-pack.
    /// 5. Object budget (>1,000,000 objects in `B(mode)`) answers 413 at upload-pack.
    /// 6. Locked `import_refs` answers 503 (`query_timeout`) at upload-pack.
    /// 7. Same-barrier upload-pack ref-race control (`LOCK TABLE git_tree IN ACCESS EXCLUSIVE MODE`
    ///    observed via `pg_stat_activity`): unchanged refs succeed with `PACK` (200),
    ///    and mutating `import_refs` at the exact same barrier rejects the pack (400).
    pub async fn code_clone_ref_budget_answers_413() {
        use std::str::FromStr;

        use git_internal::{
            hash::{HashKind, ObjectHash},
            internal::object::{
                ObjectTrait,
                tree::{Tree, TreeItem, TreeItemMode},
            },
        };

        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = Fixture::new(&db).await;
        let result = AssertUnwindSafe(async {
            let mut init = fixture.command(false, "trunk", "/third-party");
            init.args(["service", "init", "--yes"]);
            OwnedChild::spawn(&mut init).require_success().await;
            fixture.start(true, "trunk", "/third-party").await;

            fixture.git(&["init", "-q", "--initial-branch=main", "budget-work"]);
            fs::write(
                fixture.shared.path().join("budget-work/sentinel.txt"),
                SENTINEL,
            )
            .expect("budget sentinel");
            fixture.git(&["-C", "budget-work", "add", "sentinel.txt"]);
            fixture.git(&[
                "-C",
                "budget-work",
                "-c",
                "user.name=Traces Test",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "MTR13 budget seed",
            ]);
            let import_path = "/third-party/mtr94-control.git";
            let import_url = git_cli::mega2_http_url(fixture.port, import_path);
            fixture.git(&[
                "-C",
                "budget-work",
                "push",
                &import_url,
                "HEAD:refs/heads/main",
            ]);
            let main_tip = fixture.git_text(&["-C", "budget-work", "rev-parse", "HEAD"]);
            let main_tree = fixture.git_text(&["-C", "budget-work", "rev-parse", "HEAD^{tree}"]);
            let repo_id: i64 = scalar(
                &db,
                "SELECT repo_id::text AS value FROM import_refs WHERE ref_name = 'refs/heads/main' LIMIT 1",
            )
            .await
            .parse()
            .expect("repo_id");

            // 1. No-legacy >4,096 refs (4,099 refs) + >4,096 orphan git_tag rows:
            // ls-remote advertises all >=4,099 refs, and clone + fetch succeed.
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO import_refs (id, repo_id, ref_name, ref_git_id, ref_type, default_branch, created_at, updated_at) \
                     SELECT (SELECT COALESCE(MAX(id), 0) FROM import_refs) + g, {repo_id}, \
                            'refs/heads/bulk' || g::text, '{main_tip}', 'branch', false, now(), now() \
                     FROM generate_series(1, 4098) g"
                ),
            ))
            .await
            .expect("insert 4098 no-legacy refs");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO git_tag (id, repo_id, tag_id, object_id, object_type, tag_name, tagger, message, created_at, pack_id, pack_offset) \
                     SELECT (SELECT COALESCE(MAX(id), 0) FROM git_tag) + g, {repo_id}, \
                            'e' || lpad(g::text, 39, '0'), '{main_tip}', 'commit', 'orphan-' || g::text, \
                            'tagger t <t@example.invalid> 1 +0000', 'orphan tag', now(), '', 0 \
                     FROM generate_series(1, 4100) g"
                ),
            ))
            .await
            .expect("insert 4100 orphan git_tag rows");

            let mut expected_refs = BTreeMap::from([
                ("HEAD".to_owned(), main_tip.clone()),
                ("refs/heads/main".to_owned(), main_tip.clone()),
            ]);
            for index in 1..=4098 {
                expected_refs.insert(format!("refs/heads/bulk{index}"), main_tip.clone());
            }
            for protocol in ["0", "2"] {
                let protocol_config = format!("protocol.version={protocol}");
                let no_legacy_ls = fixture.git_text(&[
                    "-c", &protocol_config, "ls-remote", &import_url,
                ]);
                let mut real_refs = BTreeMap::new();
                let mut pseudo_peels = 0usize;
                for line in no_legacy_ls.lines() {
                    let mut fields = line.split_whitespace();
                    let oid = fields.next().expect("ls-remote object ID");
                    let name = fields.next().expect("ls-remote ref name");
                    assert!(fields.next().is_none(), "unexpected ls-remote record: {line}");
                    if let Some(base_ref) = name.strip_suffix("^{}") {
                        // DEBT-MTR13-V2-PEEL remains open; branch peeled:self rows
                        // do not count toward the real-ref completeness oracle.
                        assert!(expected_refs.contains_key(base_ref), "unexpected peeled base ref: {base_ref}");
                        assert_eq!(oid, main_tip, "peeled row must name the actual seeded tip");
                        pseudo_peels += 1;
                        continue;
                    }
                    assert!(
                        real_refs.insert(name.to_owned(), oid.to_owned()).is_none(),
                        "duplicate real ref in protocol {protocol} advertisement: {name}"
                    );
                }
                let real_ref_count = real_refs.len();
                for (name, expected_oid) in &expected_refs {
                    assert_eq!(
                        real_refs.remove(name).as_deref(), Some(expected_oid.as_str()),
                        "missing or wrong real ref in protocol {protocol} advertisement: {name}"
                    );
                }
                assert!(real_refs.is_empty(), "unexpected real refs in protocol {protocol}: {:?}", real_refs.keys());
                eprintln!(
                    "MTR13_NO_LEGACY_OVER_4096 requested_protocol=v{protocol} real_refs={} head=1 pseudo_peels={pseudo_peels}",
                    real_ref_count - 1
                );
            }
            fixture.git(&[
                "clone",
                "--single-branch",
                "--branch",
                "main",
                &import_url,
                "no-legacy-over4096-clone",
            ]);
            assert_eq!(
                fixture.git_text(&["-C", "no-legacy-over4096-clone", "rev-parse", "HEAD"]),
                main_tip
            );
            fixture.git(&[
                "-C",
                "no-legacy-over4096-clone",
                "fetch",
                "origin",
                "refs/heads/main",
            ]);

            // 2. Legacy ref budget (>4,096 refs with refs/libra/traces present) -> 413.
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO import_refs (id, repo_id, ref_name, ref_git_id, ref_type, default_branch, created_at, updated_at) \
                     VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM import_refs), {repo_id}, 'refs/libra/traces', '{}', 'branch', false, now(), now())",
                    "b".repeat(40)
                ),
            ))
            .await
            .expect("insert legacy traces ref");
            let ref_budget_resp = reqwest::Client::new()
                .get(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/info/refs?service=git-upload-pack",
                ))
                .send()
                .await
                .expect("legacy ref budget request");
            let ref_budget_status = ref_budget_resp.status();
            eprintln!("FIX13_ENDPOINT_413_REFS status={ref_budget_status}");
            assert_eq!(
                ref_budget_status.as_u16(),
                413,
                "an over-budget legacy ref view must answer 413"
            );

            // Remove bulk refs so ref count is 2 (<= 4,096), keeping refs/libra/traces
            // and the 4,100 git_tag rows to isolate the legacy tag budget (>4,096 tags).
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DELETE FROM import_refs WHERE repo_id = {repo_id} AND ref_name LIKE 'refs/heads/bulk%'"),
            ))
            .await
            .expect("delete bulk refs");

            // 3. Legacy tag budget (>4,096 tags in git_tag with refs/libra/traces) -> 413.
            let tag_budget_resp = reqwest::Client::new()
                .get(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/info/refs?service=git-upload-pack",
                ))
                .send()
                .await
                .expect("legacy tag budget request");
            let tag_budget_status = tag_budget_resp.status();
            eprintln!("FIX13_ENDPOINT_413_TAGS status={tag_budget_status}");
            assert_eq!(
                tag_budget_status.as_u16(),
                413,
                "an over-budget legacy tag view (>4,096 git_tag rows) must answer 413"
            );

            // Clean up synthetic git_tag rows and refs/libra/traces.
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DELETE FROM git_tag WHERE repo_id = {repo_id} AND tag_name LIKE 'orphan-%'"),
            ))
            .await
            .expect("delete orphan tags");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DELETE FROM import_refs WHERE repo_id = {repo_id} AND ref_name = 'refs/libra/traces'"),
            ))
            .await
            .expect("delete legacy traces ref");

            // 4. Commit budget (>200,000 commits in B(mode)) -> 413 at upload-pack.
            // Use a 2-level fan-out DAG (1 hub -> 201 mid commits -> 201,000 leaf commits)
            // so BFS reaches >200,000 commits in 2 levels of batch queries.
            let commit_hub = "c".repeat(40);
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO git_commit (id, repo_id, commit_id, tree, parents_id, author, committer, content, created_at, pack_id, pack_offset) \
                     SELECT (SELECT COALESCE(MAX(id), 0) FROM git_commit) + g, {repo_id}, \
                            'a' || lpad(g::text, 39, '0'), '{main_tree}', '[]'::jsonb, \
                            'author a <a@example.invalid> 1 +0000', 'committer c <c@example.invalid> 1 +0000', \
                            'leaf', now(), '', 0 \
                     FROM generate_series(1, 201000) g"
                ),
            ))
            .await
            .expect("insert 201000 leaf commits");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO git_commit (id, repo_id, commit_id, tree, parents_id, author, committer, content, created_at, pack_id, pack_offset) \
                     SELECT (SELECT COALESCE(MAX(id), 0) FROM git_commit) + m, {repo_id}, \
                            'b' || lpad(m::text, 39, '0'), '{main_tree}', \
                            (SELECT jsonb_agg('a' || lpad((((m - 1) * 1000) + k)::text, 39, '0')) FROM generate_series(1, 1000) k), \
                            'author a <a@example.invalid> 1 +0000', 'committer c <c@example.invalid> 1 +0000', \
                            'mid', now(), '', 0 \
                     FROM generate_series(1, 201) m"
                ),
            ))
            .await
            .expect("insert 201 mid commits");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO git_commit (id, repo_id, commit_id, tree, parents_id, author, committer, content, created_at, pack_id, pack_offset) \
                     VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM git_commit), {repo_id}, '{commit_hub}', '{main_tree}', \
                             (SELECT jsonb_agg('b' || lpad(m::text, 39, '0')) FROM generate_series(1, 201) m), \
                             'author a <a@example.invalid> 1 +0000', 'committer c <c@example.invalid> 1 +0000', \
                             'hub', now(), '', 0)"
                ),
            ))
            .await
            .expect("insert commit budget hub");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO import_refs (id, repo_id, ref_name, ref_git_id, ref_type, default_branch, created_at, updated_at) \
                     VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM import_refs), {repo_id}, 'refs/heads/commit-budget', '{commit_hub}', 'branch', false, now(), now())"
                ),
            ))
            .await
            .expect("insert commit-budget ref");

            let pkt_line = |line: &str| format!("{:04x}{line}", line.len() + 4);
            let commit_budget_resp = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/git-upload-pack",
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body(format!(
                    "{}0000{}",
                    pkt_line(&format!("want {commit_hub}\n")),
                    pkt_line("done\n")
                ))
                .send()
                .await
                .expect("commit budget upload-pack request");
            let commit_budget_status = commit_budget_resp.status();
            eprintln!("FIX13_ENDPOINT_413_COMMITS status={commit_budget_status}");
            assert_eq!(
                commit_budget_status.as_u16(),
                413,
                "a request exceeding 200,000 commits must answer 413 at upload-pack"
            );

            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DELETE FROM import_refs WHERE repo_id = {repo_id} AND ref_name = 'refs/heads/commit-budget'"),
            ))
            .await
            .expect("delete commit-budget ref");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DELETE FROM git_commit WHERE repo_id = {repo_id} AND commit_id <> '{main_tip}'"),
            ))
            .await
            .expect("delete synthetic commits");

            // 5. Object budget (>1,000,000 objects in B(mode)) -> 413 at upload-pack.
            // Build 1,001 child git_tree rows each referencing 1,000 distinct blob
            // OIDs (1,001,000 blobs + 1,002 trees + 1 commit = 1,002,003 > 1,000,000).
            let mut root_items = Vec::with_capacity(1001);
            let mut tree_rows_sql = Vec::with_capacity(1002);
            let base_tree_id: i64 = scalar(
                &db,
                "SELECT (COALESCE(MAX(id), 0) + 1)::text AS value FROM git_tree",
            )
            .await
            .parse()
            .expect("base_tree_id");
            for s in 0..1001usize {
                let mut items = Vec::with_capacity(1000);
                for j in 0..1000usize {
                    let blob_hex = format!("d{s:04}{j:035}");
                    items.push(TreeItem::new(
                        TreeItemMode::Blob,
                        ObjectHash::from_str(&blob_hex).expect("synthetic blob hash"),
                        format!("b{j:04}"),
                    ));
                }
                let child_tree =
                    Tree::from_tree_items_with_kind(HashKind::Sha1, items).expect("child tree");
                let child_bytes = child_tree.to_data().expect("child tree bytes");
                root_items.push(TreeItem::new(
                    TreeItemMode::Tree,
                    child_tree.id,
                    format!("s{s:04}"),
                ));
                tree_rows_sql.push(format!(
                    "({}, {repo_id}, '{}', decode('{}', 'hex'), 0, now(), '', 0)",
                    base_tree_id + s as i64,
                    child_tree.id,
                    hex::encode(child_bytes)
                ));
            }
            let obj_root_tree =
                Tree::from_tree_items_with_kind(HashKind::Sha1, root_items).expect("root tree");
            let obj_root_bytes = obj_root_tree.to_data().expect("root tree bytes");
            tree_rows_sql.push(format!(
                "({}, {repo_id}, '{}', decode('{}', 'hex'), 0, now(), '', 0)",
                base_tree_id + 1001,
                obj_root_tree.id,
                hex::encode(obj_root_bytes)
            ));
            for chunk in tree_rows_sql.chunks(250) {
                db.execute_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!(
                        "INSERT INTO git_tree (id, repo_id, tree_id, sub_trees, size, created_at, pack_id, pack_offset) VALUES {}",
                        chunk.join(", ")
                    ),
                ))
                .await
                .expect("insert synthetic git_tree batch");
            }
            let obj_hub = "f".repeat(40);
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO git_commit (id, repo_id, commit_id, tree, parents_id, author, committer, content, created_at, pack_id, pack_offset) \
                     VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM git_commit), {repo_id}, '{obj_hub}', '{}', '[]'::jsonb, \
                             'author a <a@example.invalid> 1 +0000', 'committer c <c@example.invalid> 1 +0000', \
                             'obj-hub', now(), '', 0)",
                    obj_root_tree.id
                ),
            ))
            .await
            .expect("insert object budget hub commit");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO import_refs (id, repo_id, ref_name, ref_git_id, ref_type, default_branch, created_at, updated_at) \
                     VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM import_refs), {repo_id}, 'refs/heads/obj-budget', '{obj_hub}', 'branch', false, now(), now())"
                ),
            ))
            .await
            .expect("insert obj-budget ref");

            let obj_budget_resp = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/git-upload-pack",
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body(format!(
                    "{}0000{}",
                    pkt_line(&format!("want {obj_hub}\n")),
                    pkt_line("done\n")
                ))
                .send()
                .await
                .expect("object budget upload-pack request");
            let obj_budget_status = obj_budget_resp.status();
            eprintln!("FIX13_ENDPOINT_413_OBJECTS status={obj_budget_status}");
            assert_eq!(
                obj_budget_status.as_u16(),
                413,
                "a request exceeding 1,000,000 objects must answer 413 at upload-pack"
            );

            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DELETE FROM import_refs WHERE repo_id = {repo_id} AND ref_name = 'refs/heads/obj-budget'"),
            ))
            .await
            .expect("delete obj-budget ref");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DELETE FROM git_commit WHERE repo_id = {repo_id} AND commit_id = '{obj_hub}'"),
            ))
            .await
            .expect("delete obj_hub commit");
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DELETE FROM git_tree WHERE repo_id = {repo_id} AND id >= {base_tree_id}"),
            ))
            .await
            .expect("delete synthetic git_tree rows");

            // 6. Endpoint 503 timeout control on locked import_refs.
            let lock_txn = db.begin().await.expect("lock transaction");
            sea_orm::ConnectionTrait::execute_unprepared(
                &lock_txn,
                "LOCK TABLE import_refs IN ACCESS EXCLUSIVE MODE",
            )
            .await
            .expect("exclusive lock on import_refs");
            let locked_response = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/git-upload-pack",
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body(format!(
                    "{}0000{}",
                    pkt_line(&format!("want {main_tip}\n")),
                    pkt_line("done\n")
                ))
                .send()
                .await
                .expect("locked fetch request");
            let locked_status = locked_response.status();
            lock_txn.rollback().await.expect("release ref lock");
            eprintln!("FIX13_ENDPOINT_503 status={locked_status}");
            assert_eq!(
                locked_status.as_u16(),
                503,
                "a blocked ref read must answer 503 at the endpoint"
            );

            // 7. Same-barrier upload-pack ref-race control using
            // `LOCK TABLE git_tree IN ACCESS EXCLUSIVE MODE` + `pg_stat_activity`.
            // First: positive control at the barrier (no ref mutation -> 200 + PACK).
            let ok_barrier = db.begin().await.expect("ok barrier txn");
            sea_orm::ConnectionTrait::execute_unprepared(
                &ok_barrier,
                "LOCK TABLE git_tree IN ACCESS EXCLUSIVE MODE",
            )
            .await
            .expect("lock git_tree for positive control");
            let upload_url = git_cli::mega2_host_http_url(
                fixture.port,
                "/third-party/mtr94-control.git/git-upload-pack",
            );
            let upload_body = format!(
                "{}0000{}",
                pkt_line(&format!("want {main_tip}\n")),
                pkt_line("done\n")
            );
            let ok_url = upload_url.clone();
            let ok_body = upload_body.clone();
            let ok_req = tokio::spawn(async move {
                reqwest::Client::new()
                    .post(ok_url)
                    .header("content-type", "application/x-git-upload-pack-request")
                    .body(ok_body)
                    .send()
                    .await
                    .expect("positive barrier upload-pack")
            });
            let mut parked = false;
            for _ in 0..100 {
                sea_orm::ConnectionTrait::execute_unprepared(
                    &ok_barrier,
                    "SELECT pg_stat_clear_snapshot()",
                )
                .await
                .expect("clear snapshot");
                let row = ok_barrier
                    .query_one_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                         WHERE datname = current_database() AND wait_event_type = 'Lock' \
                         AND pg_backend_pid() = ANY(pg_blocking_pids(pid))) AS v",
                    ))
                    .await
                    .expect("poll pg_stat_activity")
                    .expect("pg_stat_activity row");
                if row.try_get::<bool>("", "v").expect("bool") {
                    parked = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(
                parked,
                "upload-pack positive control must park at the git_tree barrier"
            );
            ok_barrier.rollback().await.expect("release positive barrier");
            let ok_resp = ok_req.await.expect("join positive upload-pack");
            let ok_status = ok_resp.status();
            let ok_bytes = ok_resp.bytes().await.expect("positive body");
            eprintln!(
                "FIX13_RACE_BARRIER_OK status={ok_status} bytes={}",
                ok_bytes.len()
            );
            assert_eq!(ok_status.as_u16(), 200);
            assert!(
                ok_bytes.windows(4).any(|w| w == b"PACK"),
                "unchanged ref view at the barrier must return a valid PACK"
            );

            // Second: negative control at the exact same `LOCK TABLE git_tree`
            // barrier (mutate `import_refs` while parked -> 400 and no PACK).
            let race_barrier = db.begin().await.expect("race barrier txn");
            sea_orm::ConnectionTrait::execute_unprepared(
                &race_barrier,
                "LOCK TABLE git_tree IN ACCESS EXCLUSIVE MODE",
            )
            .await
            .expect("lock git_tree for negative race control");
            let race_req = tokio::spawn(async move {
                reqwest::Client::new()
                    .post(upload_url)
                    .header("content-type", "application/x-git-upload-pack-request")
                    .body(upload_body)
                    .send()
                    .await
                    .expect("negative barrier upload-pack")
            });
            let mut race_parked = false;
            for _ in 0..100 {
                sea_orm::ConnectionTrait::execute_unprepared(
                    &race_barrier,
                    "SELECT pg_stat_clear_snapshot()",
                )
                .await
                .expect("clear snapshot");
                let row = race_barrier
                    .query_one_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                         WHERE datname = current_database() AND wait_event_type = 'Lock' \
                         AND pg_backend_pid() = ANY(pg_blocking_pids(pid))) AS v",
                    ))
                    .await
                    .expect("poll pg_stat_activity")
                    .expect("pg_stat_activity row");
                if row.try_get::<bool>("", "v").expect("bool") {
                    race_parked = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(
                race_parked,
                "upload-pack race control must park at the exact same git_tree barrier"
            );
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "INSERT INTO import_refs (id, repo_id, ref_name, ref_git_id, ref_type, default_branch, created_at, updated_at) \
                     VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM import_refs), {repo_id}, 'refs/heads/race-moved', '{main_tip}', 'branch', false, now(), now())"
                ),
            ))
            .await
            .expect("mutate import_refs while upload-pack is parked at git_tree");
            race_barrier
                .rollback()
                .await
                .expect("release negative race barrier");
            let race_resp = race_req.await.expect("join negative upload-pack");
            let race_status = race_resp.status();
            let race_bytes = race_resp.bytes().await.expect("negative body");
            let race_text = String::from_utf8_lossy(&race_bytes);
            eprintln!("FIX13_RACE_BARRIER_REJECT status={race_status} body={race_text}");
            assert_eq!(
                race_status.as_u16(),
                400,
                "mutating import_refs between authorization and pack must fail upload-pack"
            );
            assert!(
                race_text.contains("refs changed while building the pack"),
                "race failure must report ref-view change: {race_text}"
            );
            assert!(
                !race_bytes.windows(4).any(|w| w == b"PACK"),
                "rejected race response must not emit a PACK"
            );
        })
        .catch_unwind()
        .await;
        fixture.stop();
        let closed = db.close().await;
        drop(guard);
        if let Err(panic) = result {
            let logs = fixture.temp.keep();
            eprintln!("ref-budget fixture logs: {}", logs.display());
            std::panic::resume_unwind(panic);
        }
        closed.expect("close ref-budget pool");
    }

    /// AC-7/AC-8 at the code-path view across the fetch modes the card names.
    ///
    /// Every arm clones the same seeded code path and must (a) contain no
    /// trace-plane entry and (b) contain the seeded code objects. `--depth=1`
    /// additionally must not carry the parent commit. The root view is a
    /// separate question: the legacy `%2etraces` alias is served there today,
    /// so it is not asserted here.
    pub async fn code_clone_isolation_code_path_objects() {
        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = Fixture::new(&db).await;
        let result = AssertUnwindSafe(async {
            // Key before start: the SSH server loads its key table on boot.
            fixture.seed_ssh(&db).await;
            fixture.prepare(&db).await;
            let url = git_cli::mega2_http_url(fixture.port, "/project/control.git");

            let mut baseline_objects = 0usize;
            let arms: [(&str, Vec<&str>); 9] = [
                ("iso-default", vec![]),
                ("iso-bare", vec!["--bare"]),
                ("iso-no-tags", vec!["--no-tags"]),
                ("iso-mirror", vec!["--mirror"]),
                ("iso-depth1", vec!["--depth=1"]),
                ("iso-blobless", vec!["--filter=blob:none"]),
                // Explicit v0 and v2 arms: the object set and the absence of
                // trace entries must not depend on the wire protocol.
                ("iso-v0", vec!["-c", "protocol.version=0"]),
                ("iso-v2", vec!["-c", "protocol.version=2"]),
                // v2 is the only version that filters (`UPLOAD_CAP_LIST` has no
                // `filter`), so this is AC-4's real axis: blobs must be withheld.
                (
                    "iso-v2-blobless",
                    vec!["-c", "protocol.version=2", "--filter=blob:none"],
                ),
            ];
            for (dir, extra) in arms.iter() {
                let mut args = vec!["clone"];
                args.extend(extra.iter().copied());
                args.push(&url);
                args.push(dir);
                fixture.git(&args);

                let listing = fixture.git_text(&["-C", dir, "rev-list", "--objects", "--all"]);
                let traced: Vec<&str> = listing
                    .lines()
                    .filter(|line| line.to_ascii_lowercase().contains("traces"))
                    .collect();
                assert!(
                    traced.is_empty(),
                    "FIX13_AC7/8 {dir} must not contain trace entries: {traced:?}"
                );
                assert!(
                    !listing.is_empty(),
                    "FIX13_AC7 {dir} must contain the seeded code objects"
                );

                let commit_count = fixture.git_text(&["-C", dir, "rev-list", "--count", "--all"]);
                if *dir == "iso-depth1" {
                    assert_eq!(
                        commit_count, "1",
                        "FIX13_AC3 depth=1 must carry exactly one commit, got {commit_count}"
                    );
                } else if *dir == "iso-v2-blobless" {
                    // Measured: this arm receives the full closure, so v2
                    // filtering is not demonstrated here. An earlier version of
                    // this comment claimed the handler refused the filter, which
                    // was wrong. AC-4's positive case is asserted separately below
                    // on the ImportRepo path (`FIX13_AC4_IMPORT_FILTERED`).
                    let count = listing.lines().count();
                    eprintln!("FIX13_AC4_V2_BLOBLESS objects={count} full={baseline_objects}");
                    assert_eq!(
                        count, baseline_objects,
                        "FIX13_AC4 an ImportRepo must refuse `filter` and degrade to a full clone"
                    );
                } else if *dir == "iso-blobless" {
                    // `UPLOAD_CAP_LIST` advertises `shallow` but not `filter`, so
                    // a `--filter=blob:none` request is ignored by the client: it
                    // still records a promisor remote, yet the pack carries the
                    // full closure. The arm therefore has to behave exactly like a
                    // full clone rather than omit blobs; the promisor flag is
                    // recorded to show the request was made and degraded.
                    let promisor =
                        fixture.git_text(&["-C", dir, "config", "--get", "remote.origin.promisor"]);
                    eprintln!(
                        "FIX13_AC4_BLOBLESS promisor={promisor} objects={}",
                        listing.lines().count()
                    );
                    assert_eq!(
                        listing.lines().count(),
                        baseline_objects,
                        "FIX13_AC4 without a `filter` capability a blobless request must \
                         degrade to a full clone"
                    );
                } else {
                    if baseline_objects == 0 {
                        baseline_objects = listing.lines().count();
                    }
                    assert_eq!(
                        listing.lines().count(),
                        baseline_objects,
                        "FIX13_AC7 {dir} object count must match the default clone"
                    );
                }
            }

            // Object integrity per arm. `fsck` is skipped on promisor arms: it
            // lazily fetches the objects it is asked to check, which would both
            // mask a filtering bug and change the counts.
            for (dir, flags) in arms.iter() {
                if flags.iter().any(|flag| flag.contains("--filter")) {
                    eprintln!("FIX13_FSCK {dir} skipped=partial-clone");
                    continue;
                }
                fixture.git(&["-C", dir, "fsck", "--strict"]);
                eprintln!("FIX13_FSCK {dir} strict=ok");
            }

            // AC-6 oracle: every reachable object must be present in the client's
            // object database exactly once. Storage form is not asserted - the v0
            // arm keeps its objects loose rather than packed.
            for (dir, _) in arms.iter() {
                let reachable = fixture
                    .git_text(&["-C", dir, "rev-list", "--objects", "--all"])
                    .lines()
                    .count();
                let counted = fixture.git_text(&["-C", dir, "count-objects", "-v"]);
                let field = |name: &str| -> usize {
                    counted
                        .lines()
                        .find_map(|line| line.strip_prefix(name))
                        .and_then(|value| value.trim().parse().ok())
                        .unwrap_or_else(|| {
                            panic!("FIX13_AC6 {dir} missing {name} in count-objects")
                        })
                };
                let loose = field("count: ");
                let in_pack = field("in-pack: ");
                eprintln!(
                    "FIX13_AC6_ODB {dir} loose={loose} in-pack={in_pack} reachable={reachable}"
                );
                assert_eq!(
                    loose + in_pack,
                    reachable,
                    "FIX13_AC6 {dir} stored objects must equal the reachable set"
                );
            }

            // AC-4 on the card's own axis: an ImportRepo served from
            // `/third-party` must also withhold blobs over v2 blob:none.
            let import_url =
                git_cli::mega2_http_url(fixture.port, "/third-party/mtr94-control.git");
            fixture.git(&[
                "-c",
                "protocol.version=2",
                "clone",
                "--no-checkout",
                "--filter=blob:none",
                &import_url,
                "iso-import-blobless",
            ]);
            let import_types = fixture.git_text(&[
                "-C",
                "iso-import-blobless",
                "cat-file",
                "--batch-check=%(objecttype)",
                "--batch-all-objects",
            ]);
            let import_blobs = import_types
                .lines()
                .filter(|kind| kind.trim() == "blob")
                .count();
            eprintln!("FIX13_AC4_IMPORT_FILTERED blobs={import_blobs}");
            assert_eq!(
                import_blobs, 0,
                "FIX13_AC4 an ImportRepo must withhold every blob under v2 blob:none"
            );

            // AC-3 on the card's own axis: a depth-limited clone of an ImportRepo
            // must carry exactly one commit and must not hold the parent commit.
            fixture.git(&["clone", &import_url, "import-depth-work"]);
            std::fs::write(
                fixture.shared.path().join("import-depth-work/depth.txt"),
                b"FIX13 depth\n",
            )
            .expect("write depth file");
            fixture.git(&["-C", "import-depth-work", "add", "depth.txt"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "FIX13 depth advance",
            ]);
            let parent_oid = fixture.git_text(&["-C", "import-depth-work", "rev-parse", "HEAD~1"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "HEAD:refs/heads/main",
            ]);
            fixture.git(&[
                "-c",
                "protocol.version=0",
                "clone",
                "--depth=1",
                "--no-checkout",
                &import_url,
                "iso-import-depth1",
            ]);
            let depth_count =
                fixture.git_text(&["-C", "iso-import-depth1", "rev-list", "--count", "--all"]);
            assert_eq!(
                depth_count, "1",
                "FIX13_AC3 an ImportRepo depth=1 clone must carry exactly one commit"
            );
            let parent = git_cli::git_cli_no_auth(
                fixture.shared.path(),
                &["-C", "iso-import-depth1", "cat-file", "-e", &parent_oid],
            );
            eprintln!(
                "FIX13_AC3_IMPORT_DEPTH1 parent_present={} parent={parent_oid}",
                parent.status.success()
            );
            assert!(
                !parent.status.success(),
                "FIX13_AC3 the parent commit must not be present in a depth=1 clone"
            );

            // AC-3 also forbids the parent's objects, not just the commit. The
            // parent tree differs from the child's (the child added a file), so it
            // must be absent from the shallow clone as well.
            let parent_tree = fixture.git_text(&[
                "-C",
                "import-depth-work",
                "rev-parse",
                &format!("{parent_oid}^{{tree}}"),
            ]);
            let parent_tree_present = git_cli::git_cli_no_auth(
                fixture.shared.path(),
                &["-C", "iso-import-depth1", "cat-file", "-e", &parent_tree],
            )
            .status
            .success();
            eprintln!(
                "FIX13_AC3_PARENT_TREE present={parent_tree_present} tree={parent_tree}"
            );
            assert!(
                !parent_tree_present,
                "FIX13_AC3 the parent tree must not be present in a depth=1 clone"
            );

            // AC-3: a blob that exists only in the excluded parent must be absent
            // too. A file is added and then deleted, so the blob is reachable from
            // the intermediate commit but not from the tip a depth=1 clone takes.
            std::fs::write(
                fixture.shared.path().join("import-depth-work/transient.txt"),
                b"FIX13 transient\n",
            )
            .expect("write transient file");
            fixture.git(&["-C", "import-depth-work", "add", "transient.txt"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "FIX13 add transient",
            ]);
            let transient_blob = fixture.git_text(&[
                "-C",
                "import-depth-work",
                "rev-parse",
                "HEAD:transient.txt",
            ]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "HEAD:refs/heads/main",
            ]);
            fixture.git(&["-C", "import-depth-work", "rm", "-q", "transient.txt"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "FIX13 drop transient",
            ]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "HEAD:refs/heads/main",
            ]);
            fixture.git(&[
                "-c",
                "protocol.version=0",
                "clone",
                "--depth=1",
                "--no-checkout",
                &import_url,
                "iso-import-depth2",
            ]);
            let shallow_commits = fixture.git_text(&[
                "-C",
                "iso-import-depth2",
                "rev-list",
                "--count",
                "--all",
            ]);
            assert_eq!(
                shallow_commits, "1",
                "FIX13_AC3 the second shallow clone must carry exactly one commit"
            );
            let transient_present = git_cli::git_cli_no_auth(
                fixture.shared.path(),
                &["-C", "iso-import-depth2", "cat-file", "-e", &transient_blob],
            )
            .status
            .success();
            eprintln!(
                "FIX13_AC3_PARENT_BLOB present={transient_present} blob={transient_blob}"
            );
            assert!(
                !transient_present,
                "FIX13_AC3 a parent-only blob must not be present in a depth=1 clone"
            );

            // AC-3: a tag pointing at the excluded parent must not arrive either,
            // even though the ref exists and the client asks for include-tag.
            let parent_of_tip =
                fixture.git_text(&["-C", "import-depth-work", "rev-parse", "HEAD~1"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "fix13-parent-tag",
                "-m",
                "FIX13 parent tag",
                &parent_of_tip,
            ]);
            let parent_tag_oid = fixture.git_text(&[
                "-C",
                "import-depth-work",
                "rev-parse",
                "fix13-parent-tag",
            ]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "refs/tags/fix13-parent-tag",
            ]);
            fixture.git(&[
                "-c",
                "protocol.version=0",
                "clone",
                "--depth=1",
                "--no-checkout",
                &import_url,
                "iso-import-depth3",
            ]);
            let parent_tag_present = git_cli::git_cli_no_auth(
                fixture.shared.path(),
                &["-C", "iso-import-depth3", "cat-file", "-e", &parent_tag_oid],
            )
            .status
            .success();
            eprintln!(
                "FIX13_AC3_PARENT_TAG present={parent_tag_present} tag={parent_tag_oid}"
            );
            assert!(
                !parent_tag_present,
                "FIX13_AC3 a tag on the excluded parent must not be present"
            );

            // include-tag positive case on the ImportRepo path. Stock git cannot
            // distinguish it: `--no-tags` suppresses the capability and `--tags`
            // fetches the tag ref anyway. So the request is crafted by hand - a
            // want line carrying `include-tag`, no tag ref - and the returned pack
            // is indexed to see whether the tag object is inside it.
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "fix13-include-tag",
                "-m",
                "FIX13 include-tag probe",
            ]);
            let tag_oid =
                fixture.git_text(&["-C", "import-depth-work", "rev-parse", "fix13-include-tag"]);
            let want_oid = fixture.git_text(&["-C", "import-depth-work", "rev-parse", "HEAD"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "refs/tags/fix13-include-tag",
            ]);
            let tag_rows = scalar(
                &db,
                "SELECT count(*)::text AS value FROM git_tag WHERE tag_name = 'fix13-include-tag'",
            )
            .await;
            assert_eq!(
                tag_rows, "1",
                "FIX13_INCLUDE_TAG the pushed tag must be persisted in git_tag"
            );

            // AC-2: an incremental fetch should deliver exactly the objects the
            // client lacks. This compares the client's object-id set before and
            // after the fetch, so it shows the fetch added the right objects - but
            // it CANNOT detect retransmission of objects the client already owns,
            // because those never change that set. Capturing the transmitted pack
            // would be required for that, and is not done here.
            fixture.git(&["clone", &import_url, "ac2-clone"]);
            // The delta must be measured against the client's pre-fetch tip;
            // excluding the base commit instead overstates it.
            let ac2_pre_tip = fixture.git_text(&["-C", "ac2-clone", "rev-parse", "HEAD"]);
            let before_ids = fixture.git_text(&[
                "-C",
                "ac2-clone",
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ]);
            std::fs::write(
                fixture.shared.path().join("import-depth-work/incremental.txt"),
                b"FIX13 incremental\n",
            )
            .expect("write incremental file");
            fixture.git(&["-C", "import-depth-work", "add", "incremental.txt"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "FIX13 incremental advance",
            ]);
            let advance_tip = fixture.git_text(&["-C", "import-depth-work", "rev-parse", "HEAD"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "HEAD:refs/heads/main",
            ]);
            fixture.git(&["-C", "ac2-clone", "fetch", "origin"]);
            let after_ids = fixture.git_text(&[
                "-C",
                "ac2-clone",
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ]);
            let before: std::collections::HashSet<&str> = before_ids.lines().collect();
            let after: std::collections::HashSet<&str> = after_ids.lines().collect();
            let mut newly: Vec<&str> = after.difference(&before).copied().collect();
            newly.sort_unstable();
            let expected_text = fixture.git_text(&[
                "-C",
                "ac2-clone",
                "rev-list",
                "--objects",
                &advance_tip,
                "--not",
                &ac2_pre_tip,
            ]);
            let mut expected: Vec<&str> = expected_text
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .collect();
            expected.sort_unstable();
            expected.dedup();
            eprintln!(
                "FIX13_AC2_DELTA fetched={} expected={} ids={newly:?}",
                newly.len(),
                expected.len()
            );
            assert_eq!(
                newly, expected,
                "FIX13_AC2 the incremental fetch must transfer exactly the missing objects"
            );

            let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4);
            let request_body = format!(
                "{}{}{}",
                pkt(&format!("want {want_oid} include-tag side-band-64k\n")),
                "0000",
                pkt("done\n")
            );
            let post = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/git-upload-pack",
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body(request_body)
                .send()
                .await
                .expect("include-tag probe request");
            let status = post.status();
            let bytes = post.bytes().await.expect("probe body");
            // The pack arrives as side-band-64k packets: a 4-hex length, a status
            // byte (1 = pack data, 2 = progress, 3 = error), then the payload.
            let mut pack: Vec<u8> = Vec::new();
            let mut offset = 0usize;
            let mut error_line = None;
            while offset + 4 <= bytes.len() {
                let header = String::from_utf8_lossy(&bytes[offset..offset + 4]).to_string();
                offset += 4;
                if header == "0000" {
                    continue;
                }
                let Ok(len) = usize::from_str_radix(&header, 16) else {
                    break;
                };
                if len == 0 || offset + len - 4 > bytes.len() {
                    break;
                }
                let payload = &bytes[offset..offset + len - 4];
                offset += len - 4;
                match payload.first() {
                    Some(1) => pack.extend_from_slice(&payload[1..]),
                    Some(3) => {
                        error_line = Some(String::from_utf8_lossy(&payload[1..]).to_string());
                        break;
                    }
                    _ => {}
                }
            }
            let pack = if pack.starts_with(b"PACK") {
                pack
            } else {
                let start = bytes
                    .windows(4)
                    .position(|window| window == b"PACK")
                    .unwrap_or_else(|| {
                        panic!(
                            "FIX13_INCLUDE_TAG no pack in response (status {status}, {} bytes, error {error_line:?})",
                            bytes.len()
                        )
                    });
                bytes[start..].to_vec()
            };
            std::fs::write(fixture.shared.path().join("include-tag-probe.pack"), &pack)
                .expect("write probe pack");

            // The probe repository must use the same object format as the server,
            // otherwise `index-pack` rejects the captured pack for a hash mismatch.
            let probe_format =
                std::env::var("MEGA_TRACES_TEST_FORMAT").unwrap_or_else(|_| "sha1".to_owned());
            fixture.git(&[
                "init",
                "-q",
                &format!("--object-format={probe_format}"),
                "iso-include-tag",
            ]);
            fixture.git(&[
                "-C",
                "iso-include-tag",
                "index-pack",
                "../include-tag-probe.pack",
            ]);
            let listing = fixture.git_text(&[
                "-C",
                "iso-include-tag",
                "verify-pack",
                "-v",
                "../include-tag-probe.idx",
            ]);
            let tag_in_pack = listing
                .lines()
                .any(|line| line.split_whitespace().next() == Some(tag_oid.as_str()));

            // AC-1 exactness: the pack must carry the requested closure plus the
            // attached tag, and nothing else. The closure is computed from the
            // server-side work tree with the same object walk a client would see.
            let pack_ids: Vec<&str> = listing
                .lines()
                .filter_map(|line| {
                    let first = line.split_whitespace().next()?;
                    // SHA-1 ids are 40 hex chars, SHA-256 ones are 64.
                    if matches!(first.len(), 40 | 64)
                        && first.chars().all(|c| c.is_ascii_hexdigit())
                    {
                        Some(first)
                    } else {
                        None
                    }
                })
                .collect();
            let closure_text = fixture.git_text(&[
                "-C",
                "import-depth-work",
                "rev-list",
                "--objects",
                &want_oid,
            ]);
            let closure_ids: Vec<&str> = closure_text
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .collect();
            // The number of attachable tags depends on what earlier probes left
            // behind, so it is derived rather than assumed: every authorised tag
            // whose peeled target is inside the closure must be attached.
            let tag_refs = fixture.git_text(&[
                "-C",
                "import-depth-work",
                "for-each-ref",
                "--format=%(objectname) %(*objectname)",
                "refs/tags",
            ]);
            let closure_set: std::collections::HashSet<&str> =
                closure_ids.iter().copied().collect();
            let attachable: Vec<&str> = tag_refs
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .filter(|_| true)
                .collect::<Vec<_>>();
            let attachable_count = tag_refs
                .lines()
                .filter(|line| {
                    let mut parts = line.split_whitespace();
                    let _tag = parts.next();
                    let target = parts.next().unwrap_or_default();
                    closure_set.contains(target)
                })
                .count();
            let _ = attachable;
            eprintln!(
                "FIX13_AC1_EXACT pack={} closure={} attachable_tags={attachable_count}",
                pack_ids.len(),
                closure_ids.len()
            );
            assert_eq!(
                pack_ids.len(),
                closure_ids.len() + attachable_count,
                "FIX13_AC1 the pack must be the closure plus exactly the attachable tags"
            );
            for oid in &closure_ids {
                assert!(
                    pack_ids.contains(oid),
                    "FIX13_AC1 the pack is missing closure object {oid}"
                );
            }
            eprintln!(
                "FIX13_INCLUDE_TAG status={status} pack_bytes={} tag_in_pack={tag_in_pack}",
                pack.len()
            );
            assert!(
                tag_in_pack,
                "FIX13_INCLUDE_TAG the crafted include-tag request must carry the tag object"
            );

            // include-tag on the shallow path: a tag on the requested tip is inside
            // a depth-1 closure, so the crafted shallow request must carry it.
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "fix13-shallow-tag",
                "-m",
                "FIX13 shallow tag",
            ]);
            let shallow_tag_oid = fixture.git_text(&[
                "-C",
                "import-depth-work",
                "rev-parse",
                "fix13-shallow-tag",
            ]);
            let shallow_tip =
                fixture.git_text(&["-C", "import-depth-work", "rev-parse", "HEAD"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "refs/tags/fix13-shallow-tag",
            ]);
            let shallow_pack = fixture
                .crafted_shallow_include_tag_pack(
                    "/third-party/mtr94-control.git",
                    &shallow_tip,
                    1,
                )
                .await;
            std::fs::write(
                fixture.shared.path().join("shallow-include-tag.pack"),
                &shallow_pack,
            )
            .expect("write shallow pack");
            fixture.git(&[
                "-C",
                "iso-include-tag",
                "index-pack",
                "../shallow-include-tag.pack",
            ]);
            let shallow_listing = fixture.git_text(&[
                "-C",
                "iso-include-tag",
                "verify-pack",
                "-v",
                "../shallow-include-tag.idx",
            ]);
            let shallow_tag_in_pack = shallow_listing
                .lines()
                .any(|line| line.split_whitespace().next() == Some(shallow_tag_oid.as_str()));
            eprintln!(
                "FIX13_INCLUDE_TAG_SHALLOW tag_in_pack={shallow_tag_in_pack} bytes={}",
                shallow_pack.len()
            );
            assert!(
                shallow_tag_in_pack,
                "FIX13_INCLUDE_TAG_SHALLOW a tag inside the shallow closure must be attached"
            );

            // Filtered positive: a tag whose target is a retained tree must still be
            // attached when blobs are withheld, while a tag on a blob must not.
            let tree_oid = fixture.git_text(&[
                "-C",
                "import-depth-work",
                "rev-parse",
                "HEAD^{tree}",
            ]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "fix13-tree-tag",
                "-m",
                "FIX13 tree tag",
                &tree_oid,
            ]);
            let tree_tag_oid = fixture.git_text(&[
                "-C",
                "import-depth-work",
                "rev-parse",
                "fix13-tree-tag",
            ]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "refs/tags/fix13-tree-tag",
            ]);
            let filtered_tip =
                fixture.git_text(&["-C", "import-depth-work", "rev-parse", "HEAD"]);
            let filtered_pack = fixture
                .crafted_filtered_include_tag_pack_v2(
                    "/third-party/mtr94-control.git",
                    &filtered_tip,
                    "blob:none",
                )
                .await;
            std::fs::write(
                fixture.shared.path().join("filtered-include-tag.pack"),
                &filtered_pack,
            )
            .expect("write filtered pack");
            fixture.git(&[
                "-C",
                "iso-include-tag",
                "index-pack",
                "../filtered-include-tag.pack",
            ]);
            let filtered_listing = fixture.git_text(&[
                "-C",
                "iso-include-tag",
                "verify-pack",
                "-v",
                "../filtered-include-tag.idx",
            ]);
            let tree_tag_in_pack = filtered_listing
                .lines()
                .any(|line| line.split_whitespace().next() == Some(tree_tag_oid.as_str()));
            let blob_count = filtered_listing
                .lines()
                .filter(|line| line.split_whitespace().nth(1) == Some("blob"))
                .count();
            eprintln!(
                "FIX13_INCLUDE_TAG_FILTERED tag_in_pack={tree_tag_in_pack} blobs={blob_count}"
            );
            assert!(
                tree_tag_in_pack,
                "FIX13_INCLUDE_TAG_FILTERED a tag on a retained tree must be attached"
            );
            assert_eq!(
                blob_count, 0,
                "FIX13_INCLUDE_TAG_FILTERED blob:none must still withhold every blob"
            );

            // v2 end-to-end: the same crafted request over protocol v2 must also
            // deliver the authorised tag, which is what advertising `include-tag`
            // in the v2 fetch features is for.
            let v2_pack = fixture
                .crafted_include_tag_pack_v2("/third-party/mtr94-control.git", &want_oid)
                .await;
            std::fs::write(fixture.shared.path().join("v2-include-tag.pack"), &v2_pack)
                .expect("write v2 pack");
            fixture.git(&["-C", "iso-include-tag", "index-pack", "../v2-include-tag.pack"]);
            let v2_listing = fixture.git_text(&[
                "-C",
                "iso-include-tag",
                "verify-pack",
                "-v",
                "../v2-include-tag.idx",
            ]);
            let v2_tag = v2_listing
                .lines()
                .any(|line| line.split_whitespace().next() == Some(tag_oid.as_str()));
            eprintln!("FIX13_INCLUDE_TAG_V2 tag_in_pack={v2_tag}");
            assert!(
                v2_tag,
                "FIX13_INCLUDE_TAG_V2 the v2 crafted request must attach the tag too"
            );

            // AC-5 negative: once the tag ref is deleted, the leftover row must not
            // be attached even though its target is still inside the closure.
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                ":refs/tags/fix13-include-tag",
            ]);
            let residue_rows = scalar(
                &db,
                "SELECT count(*)::text AS value FROM git_tag WHERE tag_name = 'fix13-include-tag'",
            )
            .await;
            let second = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/git-upload-pack",
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body({
                    let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4);
                    format!(
                        "{}{}{}",
                        pkt(&format!("want {want_oid} include-tag side-band-64k\n")),
                        "0000",
                        pkt("done\n")
                    )
                })
                .send()
                .await
                .expect("second include-tag probe");
            let second_bytes = second.bytes().await.expect("second probe body");
            let mut pack: Vec<u8> = Vec::new();
            let mut offset = 0usize;
            while offset + 4 <= second_bytes.len() {
                let header =
                    String::from_utf8_lossy(&second_bytes[offset..offset + 4]).to_string();
                offset += 4;
                if header == "0000" {
                    continue;
                }
                let Ok(len) = usize::from_str_radix(&header, 16) else {
                    break;
                };
                if len == 0 || offset + len - 4 > second_bytes.len() {
                    break;
                }
                let payload = &second_bytes[offset..offset + len - 4];
                offset += len - 4;
                if payload.first() == Some(&1) {
                    pack.extend_from_slice(&payload[1..]);
                }
            }
            std::fs::write(
                fixture.shared.path().join("deleted-tag-probe.pack"),
                &pack,
            )
            .expect("write deleted-tag pack");
            fixture.git(&[
                "-C",
                "iso-include-tag",
                "index-pack",
                "../deleted-tag-probe.pack",
            ]);
            let second_listing = fixture.git_text(&[
                "-C",
                "iso-include-tag",
                "verify-pack",
                "-v",
                "../deleted-tag-probe.idx",
            ]);
            let tag_after_delete = second_listing
                .lines()
                .any(|line| line.split_whitespace().next() == Some(tag_oid.as_str()));
            eprintln!(
                "FIX13_INCLUDE_TAG_DELETED residue_rows={residue_rows} tag_in_pack={tag_after_delete}"
            );
            assert!(
                !tag_after_delete,
                "FIX13_INCLUDE_TAG a tag whose ref was deleted must not be attached"
            );

            // AC-5: a tag pointing at a commit outside the requested closure must
            // not be attached, even though its ref exists and is authorised.
            fixture.git(&[
                "-C",
                "import-depth-work",
                "checkout",
                "-q",
                "-b",
                "fix13-side",
            ]);
            std::fs::write(
                fixture.shared.path().join("import-depth-work/side.txt"),
                b"FIX13 side\n",
            )
            .expect("write side file");
            fixture.git(&["-C", "import-depth-work", "add", "side.txt"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "FIX13 side commit",
            ]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "HEAD:refs/heads/fix13-side",
            ]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "fix13-outside",
                "-m",
                "FIX13 outside closure",
            ]);
            let outside_oid =
                fixture.git_text(&["-C", "import-depth-work", "rev-parse", "fix13-outside"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "refs/tags/fix13-outside",
            ]);

            let outside_bytes = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/git-upload-pack",
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body({
                    let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4);
                    format!(
                        "{}{}{}",
                        pkt(&format!("want {want_oid} include-tag side-band-64k\n")),
                        "0000",
                        pkt("done\n")
                    )
                })
                .send()
                .await
                .expect("outside-closure probe");
            let outside_raw = outside_bytes.bytes().await.expect("outside probe body");
            let mut pack: Vec<u8> = Vec::new();
            let mut offset = 0usize;
            while offset + 4 <= outside_raw.len() {
                let header = String::from_utf8_lossy(&outside_raw[offset..offset + 4]).to_string();
                offset += 4;
                if header == "0000" {
                    continue;
                }
                let Ok(len) = usize::from_str_radix(&header, 16) else {
                    break;
                };
                if len == 0 || offset + len - 4 > outside_raw.len() {
                    break;
                }
                let payload = &outside_raw[offset..offset + len - 4];
                offset += len - 4;
                if payload.first() == Some(&1) {
                    pack.extend_from_slice(&payload[1..]);
                }
            }
            std::fs::write(
                fixture.shared.path().join("outside-tag-probe.pack"),
                &pack,
            )
            .expect("write outside-tag pack");
            fixture.git(&[
                "-C",
                "iso-include-tag",
                "index-pack",
                "../outside-tag-probe.pack",
            ]);
            let outside_listing = fixture.git_text(&[
                "-C",
                "iso-include-tag",
                "verify-pack",
                "-v",
                "../outside-tag-probe.idx",
            ]);
            let outside_in_pack = outside_listing
                .lines()
                .any(|line| line.split_whitespace().next() == Some(outside_oid.as_str()));
            eprintln!("FIX13_INCLUDE_TAG_OUTSIDE tag_in_pack={outside_in_pack}");
            assert!(
                !outside_in_pack,
                "FIX13_INCLUDE_TAG a tag outside the closure must not be attached"
            );

            // AC-5: a nested chain (`outer -> inner -> commit`) must attach both
            // tag objects, and must do so regardless of row order.
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "fix13-inner",
                "-m",
                "FIX13 inner",
                &want_oid,
            ]);
            let inner_oid =
                fixture.git_text(&["-C", "import-depth-work", "rev-parse", "fix13-inner"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "fix13-outer",
                "-m",
                "FIX13 outer",
                &inner_oid,
            ]);
            let outer_oid =
                fixture.git_text(&["-C", "import-depth-work", "rev-parse", "fix13-outer"]);
            fixture.git(&[
                "-C",
                "import-depth-work",
                "push",
                &import_url,
                "refs/tags/fix13-inner",
                "refs/tags/fix13-outer",
            ]);
            // The previous pushes left a side branch and an outside tag; fetch the
            // same commit again so the comparison is only about the chain.
            let nested_pack = fixture
                .crafted_include_tag_pack("/third-party/mtr94-control.git", &want_oid)
                .await;
            std::fs::write(fixture.shared.path().join("nested-tag.pack"), &nested_pack)
                .expect("write nested pack");
            fixture.git(&["-C", "iso-include-tag", "index-pack", "../nested-tag.pack"]);
            let nested_listing = fixture.git_text(&[
                "-C",
                "iso-include-tag",
                "verify-pack",
                "-v",
                "../nested-tag.idx",
            ]);
            let has = |oid: &str| {
                nested_listing
                    .lines()
                    .any(|line| line.split_whitespace().next() == Some(oid))
            };
            let inner_in = has(&inner_oid);
            let outer_in = has(&outer_oid);
            eprintln!("FIX13_INCLUDE_TAG_NESTED inner={inner_in} outer={outer_in}");
            assert!(
                inner_in && outer_in,
                "FIX13_INCLUDE_TAG a nested chain must attach both tag objects"
            );

            // VER-1's SSH dimension: SSH is a separate service process, so the

            // Endpoint-level 503: while the ref view is locked the service cannot
            // read it, and the request must answer 503 rather than 400 or a hang.
            // Previously only the status mapping was proven, not the endpoint.
            let lock_txn = db.begin().await.expect("lock transaction");
            sea_orm::ConnectionTrait::execute_unprepared(
                &lock_txn,
                "LOCK TABLE import_refs IN ACCESS EXCLUSIVE MODE",
            )
            .await
            .expect("exclusive lock on import_refs");
            let locked_response = reqwest::Client::new()
                .post(git_cli::mega2_host_http_url(
                    fixture.port,
                    "/third-party/mtr94-control.git/git-upload-pack",
                ))
                .header("content-type", "application/x-git-upload-pack-request")
                .body({
                    let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4);
                    format!("{}{}", pkt(&format!("want {want_oid}\n")), "0000",)
                })
                .send()
                .await
                .expect("locked fetch request");
            let locked_status = locked_response.status();
            lock_txn.rollback().await.expect("release ref lock");
            eprintln!("FIX13_ENDPOINT_503 status={locked_status}");
            assert_eq!(
                locked_status.as_u16(),
                503,
                "a blocked ref read must answer 503 at the endpoint"
            );

            // AC-7's control: the code clone must not change when trace data is
            // actually added between the two observations. The fixture seeds the
            // stream with `tip_oid = NULL`, so it is populated here and the same
            // code path is cloned again.
            let code_before = fixture
                .git_text(&["-C", "iso-default", "rev-list", "--objects", "--all"])
                .lines()
                .count();
            let populated = scalar(
                &db,
                &format!(
                    "UPDATE trace_stream SET tip_oid = '{}' WHERE id = '{}' RETURNING id AS value",
                    want_oid, fixture.scope
                ),
            )
            .await;
            assert_eq!(
                populated, fixture.scope,
                "FIX13_AC7 the trace stream must be populated for the comparison"
            );
            fixture.git(&["clone", &url, "iso-traces-populated"]);
            let after_listing =
                fixture.git_text(&["-C", "iso-traces-populated", "rev-list", "--objects", "--all"]);
            let code_after = after_listing.lines().count();
            let populated_traced = after_listing
                .lines()
                .any(|line| line.to_ascii_lowercase().contains("traces"));
            eprintln!(
                "FIX13_AC7_ADDED before={code_before} after={code_after} traced={populated_traced}"
            );
            assert_eq!(
                code_after, code_before,
                "FIX13_AC7 adding trace data must not change the code clone"
            );
            assert!(
                !populated_traced,
                "FIX13_AC8 the code clone must not carry trace entries after data was added"
            );

            // fixture is switched over only after every HTTP arm has run.
            fixture.start_ssh(true, false).await;
            let ssh_clone = fixture.ssh_git(&[
                "-c",
                "protocol.version=0",
                "clone",
                &fixture.ssh_url("/project/control.git"),
                "iso-ssh",
            ]);
            assert!(
                ssh_clone.status.success(),
                "FIX13_SSH ssh clone failed: {}",
                String::from_utf8_lossy(&ssh_clone.stderr)
            );
            let ssh_listing =
                fixture.git_text(&["-C", "iso-ssh", "rev-list", "--objects", "--all"]);
            let ssh_traced: Vec<&str> = ssh_listing
                .lines()
                .filter(|line| line.to_ascii_lowercase().contains("traces"))
                .collect();
            eprintln!(
                "FIX13_SSH objects={} traces={}",
                ssh_listing.lines().count(),
                ssh_traced.len()
            );
            assert!(
                ssh_traced.is_empty(),
                "FIX13_SSH the code clone must not contain trace entries: {ssh_traced:?}"
            );

            // The SSH transport must behave like HTTP across modes, not just for a
            // plain clone: bare and depth-limited clones are checked too.
            for (dir, mode_flags) in [
                ("iso-ssh-bare", vec!["--bare"]),
                ("iso-ssh-depth1", vec!["--depth=1"]),
                ("iso-ssh-no-tags", vec!["--no-tags"]),
                ("iso-ssh-blobless", vec!["--filter=blob:none"]),
                (
                    "iso-ssh-v2-blobless",
                    vec!["-c", "protocol.version=2", "--filter=blob:none"],
                ),
            ] {
                // `-c` options must precede the subcommand, while clone flags like
                // `--depth=1` must follow it. Split the mode flags accordingly.
                let mut args: Vec<String> = vec!["-c".into(), "protocol.version=0".into()];
                let mut clone_flags: Vec<String> = Vec::new();
                let mut index = 0;
                while index < mode_flags.len() {
                    if mode_flags[index] == "-c" {
                        args.push("-c".into());
                        args.push(mode_flags[index + 1].into());
                        index += 2;
                    } else {
                        clone_flags.push(mode_flags[index].into());
                        index += 1;
                    }
                }
                args.push("clone".into());
                args.extend(clone_flags);
                args.push(fixture.ssh_url("/project/control.git"));
                args.push(dir.into());
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let clone = fixture.ssh_git(&arg_refs);
                assert!(
                    clone.status.success(),
                    "FIX13_SSH {dir} clone failed: {}",
                    String::from_utf8_lossy(&clone.stderr)
                );
                let listing = fixture.git_text(&["-C", dir, "rev-list", "--objects", "--all"]);
                let traced: Vec<&str> = listing
                    .lines()
                    .filter(|line| line.to_ascii_lowercase().contains("traces"))
                    .collect();
                let commits = fixture.git_text(&["-C", dir, "rev-list", "--count", "--all"]);
                eprintln!(
                    "FIX13_SSH_MODE {dir} objects={} commits={commits} traces={}",
                    listing.lines().count(),
                    traced.len()
                );
                assert!(
                    traced.is_empty(),
                    "FIX13_SSH {dir} must not contain trace entries: {traced:?}"
                );
                if dir == "iso-ssh-depth1" {
                    assert_eq!(commits, "1", "FIX13_SSH depth=1 over SSH must carry one commit");
                }
                // The SSH arms must not diverge from the HTTP baseline either: the
                // code view carries the same objects regardless of transport.
                assert_eq!(
                    listing.lines().count(),
                    baseline_objects,
                    "FIX13_SSH {dir} object count must match the HTTP default arm"
                );
            }
        })
        .catch_unwind()
        .await;
        fixture.stop();
        let closed = db.close().await;
        drop(guard);
        if let Err(panic) = result {
            let logs = fixture.temp.keep();
            eprintln!("code-clone isolation fixture logs: {}", logs.display());
            std::panic::resume_unwind(panic);
        }
        closed.expect("close isolation pool");
    }

    pub async fn code_clone_isolation_exact_pack_matrix() {
        for format in ["sha1", "sha256"] {
            verify_import_repo_exact_pack_matrix(format).await;
        }
    }

    async fn verify_import_repo_exact_pack_matrix(format: &str) {
        const TRACE_SENTINEL: &[u8] = b"MTR13_UNIQUE_TRACES_ONLY_SENTINEL_PAYLOAD\n";

        fn batch_objects(mut bytes: &[u8]) -> BTreeMap<String, (String, Vec<u8>)> {
            let mut objects = BTreeMap::new();
            while !bytes.is_empty() {
                let newline = bytes
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .expect("batch header");
                let header = std::str::from_utf8(&bytes[..newline]).expect("batch header UTF-8");
                let fields: Vec<_> = header.split_whitespace().collect();
                assert_eq!(
                    fields.len(),
                    3,
                    "unexpected cat-file batch header: {header}"
                );
                let size: usize = fields[2].parse().expect("batch object size");
                bytes = &bytes[newline + 1..];
                assert!(
                    bytes.len() > size && bytes[size] == b'\n',
                    "truncated batch object"
                );
                assert!(
                    objects
                        .insert(
                            fields[0].to_owned(),
                            (fields[1].to_owned(), bytes[..size].to_vec())
                        )
                        .is_none(),
                    "duplicate batch object {}",
                    fields[0]
                );
                bytes = &bytes[size + 1..];
            }
            objects
        }

        #[derive(Clone, Default)]
        struct ClientBefore {
            pack_fingerprints: BTreeMap<PathBuf, String>,
            odb: BTreeSet<String>,
        }

        fn pack_fingerprints(root: &Path, bare: bool) -> BTreeMap<PathBuf, String> {
            let pack_dir = root.join(if bare {
                "objects/pack"
            } else {
                ".git/objects/pack"
            });
            if !pack_dir.exists() {
                return BTreeMap::new();
            }
            fs::read_dir(&pack_dir)
                .expect("read pre-request pack directory")
                .map(|entry| entry.expect("pre-request pack entry").path())
                .filter(|path| {
                    matches!(
                        path.extension().and_then(|ext| ext.to_str()),
                        Some("pack" | "idx")
                    )
                })
                .map(|path| {
                    let hash = hex::encode(Sha256::digest(
                        fs::read(&path).expect("read pre-request pack or index"),
                    ));
                    (path, hash)
                })
                .collect()
        }

        let (guard, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = Fixture::new(&db).await;
        fixture.object_format = format.to_owned();
        let result = AssertUnwindSafe(async {
            fixture.seed_ssh(&db).await;
            let mut init = fixture.command(false, "trunk", "/third-party");
            init.args(["service", "init", "--yes"]);
            OwnedChild::spawn(&mut init).require_success().await;
            fixture.start(true, "trunk", "/third-party").await;

            let repo_path = "/third-party/mtr13-iso.git";
            let http_url = git_cli::mega2_http_url(fixture.port, repo_path);

            fixture.git(&[
                "init",
                "-q",
                "--initial-branch=main",
                &format!("--object-format={format}"),
                "mtr13-work",
            ]);
            let work_dir = fixture.shared.path().join("mtr13-work");
            fs::create_dir_all(work_dir.join("sub")).expect("create sub dir");
            fs::write(
                work_dir.join("sub/transient.txt"),
                b"MTR13 transient parent-only blob\n",
            )
            .expect("write transient.txt");
            fs::write(
                work_dir.join("shared.txt"),
                b"MTR13 shared code and trace blob\n",
            )
            .expect("write shared.txt");
            fixture.git(&["-C", "mtr13-work", "add", "."]);
            fixture.git(&[
                "-C",
                "mtr13-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "MTR13 c1 base",
            ]);
            let c1 = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD"]);
            let t1 = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD^{tree}"]);
            let sub1 = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:sub"]);
            let b_transient =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:sub/transient.txt"]);
            let b_shared =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:shared.txt"]);

            fixture.git(&[
                "-C",
                "mtr13-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "v1-parent",
                "-m",
                "tag on c1",
                &c1,
            ]);
            let v1_parent_oid =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v1-parent"]);

            // Push c1 + v1-parent first and create loose-object incremental base clones at c1.
            fixture.git(&[
                "-C",
                "mtr13-work",
                "push",
                &http_url,
                "HEAD:refs/heads/main",
                "refs/tags/v1-parent",
            ]);
            fixture.git(&[
                "-c",
                "fetch.unpackLimit=999999",
                "-c",
                "transfer.unpackLimit=999999",
                "clone",
                "--single-branch",
                "--branch",
                "main",
                &http_url,
                "inc-base",
            ]);
            fixture.git(&[
                "-c",
                "fetch.unpackLimit=999999",
                "-c",
                "transfer.unpackLimit=999999",
                "clone",
                "--single-branch",
                "--branch",
                "main",
                "--no-tags",
                &http_url,
                "inc-base-notags",
            ]);

            // Commit 2 (c2) on main: remove transient.txt, add sub/keep.txt and readme.txt.
            fixture.git(&["-C", "mtr13-work", "rm", "-q", "sub/transient.txt"]);
            fs::create_dir_all(work_dir.join("sub")).expect("recreate sub dir");
            fs::write(work_dir.join("sub/keep.txt"), b"MTR13 keep in c2\n")
                .expect("write keep.txt");
            fs::write(work_dir.join("readme.txt"), b"MTR13 readme in c2\n")
                .expect("write readme.txt");
            fixture.git(&["-C", "mtr13-work", "add", "."]);
            fixture.git(&[
                "-C",
                "mtr13-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "MTR13 c2 tip",
            ]);
            let c2 = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD"]);
            let t2 = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD^{tree}"]);
            let sub2 = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:sub"]);
            let b_keep =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:sub/keep.txt"]);
            let b_readme =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:readme.txt"]);

            for (name, target, msg) in [
                ("v2-commit", c2.as_str(), "tag on c2 commit"),
                ("v2-nested-inner", c2.as_str(), "inner nested tag on c2"),
                ("v2-tree", t2.as_str(), "tag on c2 root tree"),
                ("v2-blob", b_readme.as_str(), "tag on c2 readme blob"),
                ("v-deleted", c2.as_str(), "tag whose ref will be deleted"),
            ] {
                fixture.git(&[
                    "-C",
                    "mtr13-work",
                    "-c",
                    "user.name=Traces Fixture",
                    "-c",
                    "user.email=traces@example.invalid",
                    "-c",
                    "tag.gpgsign=false",
                    "tag",
                    "-a",
                    name,
                    "-m",
                    msg,
                    target,
                ]);
            }
            let v2_commit_oid =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v2-commit"]);
            let v2_inner_oid =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v2-nested-inner"]);
            fixture.git(&[
                "-C",
                "mtr13-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "v2-nested-outer",
                "-m",
                "outer nested tag",
                &v2_inner_oid,
            ]);
            let v2_outer_oid =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v2-nested-outer"]);
            let v2_tree_oid = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v2-tree"]);
            let v2_blob_oid = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v2-blob"]);
            let v_deleted_oid =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v-deleted"]);

            // Unrelated side branch with its own annotated tag `v-outside`.
            fixture.git(&["-C", "mtr13-work", "checkout", "-q", "--orphan", "side"]);
            fixture.git(&["-C", "mtr13-work", "rm", "-rf", "-q", "."]);
            fs::write(work_dir.join("side.txt"), b"MTR13 side branch\n").expect("write side.txt");
            fixture.git(&["-C", "mtr13-work", "add", "side.txt"]);
            fixture.git(&[
                "-C",
                "mtr13-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "MTR13 side commit",
            ]);
            let c_side = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD"]);
            let t_side = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD^{tree}"]);
            let b_side = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:side.txt"]);
            fixture.git(&[
                "-C",
                "mtr13-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "v-outside",
                "-m",
                "tag on side branch",
                &c_side,
            ]);
            let v_outside_oid =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v-outside"]);

            fixture.git(&[
                "-C",
                "mtr13-work",
                "push",
                &http_url,
                &format!("{c2}:refs/heads/main"),
                "refs/tags/v2-commit",
                "refs/tags/v2-nested-inner",
                "refs/tags/v2-nested-outer",
                "refs/tags/v2-tree",
                "refs/tags/v2-blob",
                "refs/tags/v-outside",
                "refs/tags/v-deleted",
            ]);
            // Delete refs/tags/v-deleted so its row in git_tag is unreferenced.
            fixture.git(&[
                "-C",
                "mtr13-work",
                "push",
                &http_url,
                ":refs/tags/v-deleted",
            ]);

            let set_of = |items: &[&str]| -> BTreeSet<String> {
                items.iter().map(|s| (*s).to_owned()).collect()
            };
            let union_of = |a: &BTreeSet<String>, b: &BTreeSet<String>| -> BTreeSet<String> {
                a.union(b).cloned().collect()
            };

            let c1_objs = set_of(&[&c1, &t1, &sub1, &b_transient, &b_shared]);
            let c2_only_objs = set_of(&[&c2, &t2, &sub2, &b_keep, &b_readme]);
            let main_objs = union_of(&c1_objs, &c2_only_objs);
            let side_objs = set_of(&[&c_side, &t_side, &b_side]);
            let main_tags = set_of(&[
                &v1_parent_oid,
                &v2_commit_oid,
                &v2_inner_oid,
                &v2_outer_oid,
                &v2_tree_oid,
                &v2_blob_oid,
            ]);
            let main_with_tags = union_of(&main_objs, &main_tags);
            let c2_shallow_objs = set_of(&[&c2, &t2, &sub2, &b_keep, &b_readme, &b_shared]);
            let c2_shallow_tags = set_of(&[
                &v2_commit_oid,
                &v2_inner_oid,
                &v2_outer_oid,
                &v2_tree_oid,
                &v2_blob_oid,
            ]);
            let shallow_with_tags = union_of(&c2_shallow_objs, &c2_shallow_tags);
            let main_blobless_objs = set_of(&[&c1, &c2, &t1, &sub1, &t2, &sub2]);
            let main_blobless_tags = set_of(&[
                &v1_parent_oid,
                &v2_commit_oid,
                &v2_inner_oid,
                &v2_outer_oid,
                &v2_tree_oid,
            ]);
            let blobless_v2_with_tags = union_of(&main_blobless_objs, &main_blobless_tags);
            let mirror_all_objs = {
                let mut m = union_of(&main_with_tags, &side_objs);
                m.insert(v_outside_oid.clone());
                m
            };
            let inc_with_tags = union_of(&c2_only_objs, &c2_shallow_tags);

            let source_objects = batch_objects(&fixture.git(&[
                "-C", "mtr13-work", "cat-file", "--batch-all-objects", "--batch",
            ]));
            let wire_packs = std::cell::RefCell::new(BTreeMap::<String, PathBuf>::new());
            let client_before = std::cell::RefCell::new(BTreeMap::<String, ClientBefore>::new());
            let verify_pack_and_odb = |fixture: &Fixture,
                                       dir: &str,
                                       bare: bool,
                                       expected_pack: &BTreeSet<String>,
                                       expected_odb: &BTreeSet<String>,
                                       forbidden: &[&str],
                                       check_fsck: bool,
                                       check_c2_tags: bool,
                                       has_blob_tag: bool| {
                let pack_dir = fixture.shared.path().join(dir).join(if bare {
                    "objects/pack"
                } else {
                    ".git/objects/pack"
                });
                let before = client_before.borrow().get(dir).cloned().unwrap_or_default();
                for (path, expected_hash) in &before.pack_fingerprints {
                    let bytes = fs::read(path).expect("original pack or index must remain present");
                    assert_eq!(
                        hex::encode(Sha256::digest(bytes)), *expected_hash,
                        "MTR13_OLD_PACK {format} {dir} original pack/index changed: {}", path.display()
                    );
                }
                assert_eq!(
                    before.odb.union(expected_pack).cloned().collect::<BTreeSet<_>>(), *expected_odb,
                    "MTR13_ODB_UNION {format} {dir} expected ODB must be request-before ODB union exact received E"
                );
                let mut pack_files: Vec<PathBuf> = fs::read_dir(&pack_dir)
                    .unwrap_or_else(|e| panic!("read pack dir {}: {e}", pack_dir.display()))
                    .filter_map(|entry| {
                        let path = entry.ok()?.path();
                        if path.extension().and_then(|s| s.to_str()) == Some("pack")
                            && !before.pack_fingerprints.contains_key(&path)
                        {
                            Some(path)
                        } else {
                            None
                        }
                    })
                    .collect();
                pack_files.sort();
                assert_eq!(
                    pack_files.len(),
                    1,
                    "MTR13_EXACT_PACK {format} {dir} must receive exactly 1 new .pack file, found {pack_files:?}"
                );
                let pack_path = &pack_files[0];
                let pack_bytes = fs::read(pack_path).expect("read .pack bytes");
                assert!(
                    pack_bytes.len() >= 12 && &pack_bytes[0..4] == b"PACK",
                    "MTR13_EXACT_PACK {format} {dir} invalid PACK magic"
                );
                if let Some(wire_path) = wire_packs.borrow().get(dir) {
                    let wire_bytes = fs::read(wire_path).expect("read received wire PACK");
                    assert!(
                        wire_bytes == pack_bytes,
                        "MTR13_WIRE_PACK {format} {dir} received bytes differ from the sole indexed PACK: wire={} stored={}",
                        wire_bytes.len(), pack_bytes.len()
                    );
                } else {
                    assert!(dir.ends_with("-pre-trace"), "matrix case {dir} lacks wire capture");
                }
                let version = u32::from_be_bytes(pack_bytes[4..8].try_into().unwrap());
                let header_count =
                    u32::from_be_bytes(pack_bytes[8..12].try_into().unwrap()) as usize;
                assert_eq!(version, 2, "MTR13_EXACT_PACK {format} {dir} pack version");
                assert_eq!(
                    header_count,
                    expected_pack.len(),
                    "MTR13_EXACT_PACK {format} {dir} 12-byte PACK header count ({header_count}) != expected ({})",
                    expected_pack.len()
                );
                assert!(
                    !pack_bytes
                        .windows(TRACE_SENTINEL.len())
                        .any(|w| w == TRACE_SENTINEL),
                    "MTR13_AC8 {format} {dir} raw .pack bytes leaked trace sentinel payload"
                );

                let idx_name = pack_path
                    .with_extension("idx")
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let rel_idx = if bare {
                    format!("objects/pack/{idx_name}")
                } else {
                    format!(".git/objects/pack/{idx_name}")
                };
                let verify_out = fixture.git_text(&["-C", dir, "verify-pack", "-v", &rel_idx]);
                let verify_vec: Vec<String> = verify_out
                    .lines()
                    .filter_map(|line| {
                        let first = line.split_whitespace().next()?;
                        if matches!(first.len(), 40 | 64)
                            && first.chars().all(|c| c.is_ascii_hexdigit())
                        {
                            Some(first.to_owned())
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(
                    verify_vec.len(),
                    header_count,
                    "MTR13_EXACT_PACK {format} {dir} verify-pack entry count != 12-byte header count"
                );
                let verify_set: BTreeSet<String> = verify_vec.into_iter().collect();
                assert_eq!(
                    &verify_set, expected_pack,
                    "MTR13_EXACT_PACK {format} {dir} verify-pack object set mismatch"
                );

                let odb_out = fixture.git_text(&[
                    "-C",
                    dir,
                    "cat-file",
                    "--batch-all-objects",
                    "--batch-check=%(objectname)",
                ]);
                let odb_set: BTreeSet<String> = odb_out
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_owned())
                    .collect();
                assert_eq!(
                    &odb_set, expected_odb,
                    "MTR13_AC6 {format} {dir} client ODB object set mismatch"
                );
                let odb_objects = batch_objects(&fixture.git(&[
                    "-C", dir, "cat-file", "--batch-all-objects", "--batch",
                ]));
                assert_eq!(odb_objects.keys().cloned().collect::<BTreeSet<_>>(), *expected_odb);
                for (oid, object) in &odb_objects {
                    assert_eq!(Some(object), source_objects.get(oid), "MTR13_OBJECT_BYTES {format} {dir} {oid}");
                    assert!(
                        !object.1.windows(TRACE_SENTINEL.len()).any(|bytes| bytes == TRACE_SENTINEL),
                        "MTR13_AC8 {format} {dir} decoded object {oid} contains trace sentinel"
                    );
                }
                for bad in forbidden {
                    assert!(
                        !verify_set.contains(*bad) && !odb_set.contains(*bad),
                        "MTR13_ISOLATION {format} {dir} leaked forbidden object {bad}"
                    );
                }
                eprintln!(
                    "MTR13_CASE_OK format={format} case={dir} pack_objects={} odb_objects={} pack_sha256={}",
                    verify_set.len(), odb_set.len(), hex::encode(Sha256::digest(&pack_bytes))
                );
                if check_fsck {
                    fixture.git(&["-C", dir, "fsck", "--strict"]);
                }
                if check_c2_tags {
                    assert_eq!(
                        fixture.git_text(&["-C", dir, "rev-parse", "v2-commit^{}"]),
                        c2,
                        "MTR13_AC5 {format} {dir} peel v2-commit"
                    );
                    assert_eq!(
                        fixture.git_text(&["-C", dir, "rev-parse", "v2-nested-outer^{}"]),
                        c2,
                        "MTR13_AC5 {format} {dir} peel v2-nested-outer"
                    );
                    assert_eq!(
                        fixture.git_text(&["-C", dir, "rev-parse", "v2-tree^{}"]),
                        t2,
                        "MTR13_AC5 {format} {dir} peel v2-tree"
                    );
                    if has_blob_tag {
                        assert_eq!(
                            fixture.git_text(&["-C", dir, "rev-parse", "v2-blob^{}"]),
                            b_readme,
                            "MTR13_AC5 {format} {dir} peel v2-blob"
                        );
                    }
                }
            };

            // AC-7 baseline clone BEFORE publishing any trace data.
            let pre_trace_dir = format!("mtr13-{format}-pre-trace");
            fixture.git(&[
                "-c",
                "fetch.unpackLimit=1",
                "-c",
                "transfer.unpackLimit=1",
                "clone",
                "--single-branch",
                "--branch",
                "main",
                &http_url,
                &pre_trace_dir,
            ]);
            verify_pack_and_odb(
                &fixture,
                &pre_trace_dir,
                false,
                &main_with_tags,
                &main_with_tags,
                &[&c_side, &v_outside_oid, &v_deleted_oid],
                true,
                true,
                true,
            );

            // Publish trace data on both legacy `refs/libra/traces` (sharing `b_shared`
            // with code while adding `TRACE_SENTINEL`) and `.traces` (`trace_stream.tip_oid`).
            fixture.git(&[
                "-C",
                "mtr13-work",
                "checkout",
                "-q",
                "--orphan",
                "trace-orphan",
            ]);
            fixture.git(&["-C", "mtr13-work", "rm", "-rf", "-q", "."]);
            fs::write(
                work_dir.join("shared.txt"),
                b"MTR13 shared code and trace blob\n",
            )
            .expect("write shared trace blob");
            fs::write(work_dir.join("trace-only.txt"), TRACE_SENTINEL)
                .expect("write trace sentinel blob");
            fixture.git(&["-C", "mtr13-work", "add", "."]);
            fixture.git(&[
                "-C",
                "mtr13-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "MTR13 legacy trace commit",
            ]);
            let c_trace = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD"]);
            let t_trace = fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD^{tree}"]);
            let b_trace =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:trace-only.txt"]);
            let b_trace_shared =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "HEAD:shared.txt"]);
            assert_eq!(
                b_trace_shared, b_shared,
                "trace history must share b_shared with code"
            );
            fixture.git(&[
                "-C",
                "mtr13-work",
                "-c",
                "user.name=Traces Fixture",
                "-c",
                "user.email=traces@example.invalid",
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "v-trace-tag",
                "-m",
                "tag on trace commit",
                &c_trace,
            ]);
            let v_trace_tag_oid =
                fixture.git_text(&["-C", "mtr13-work", "rev-parse", "v-trace-tag"]);
            fixture.git(&[
                "-C",
                "mtr13-work",
                "push",
                "--no-thin",
                &http_url,
                &format!("{c_trace}:refs/libra/traces"),
                "refs/tags/v-trace-tag",
            ]);
            fixture.git(&[
                "-C",
                "mtr13-work",
                "push",
                &http_url,
                ":refs/tags/v-trace-tag",
            ]);

            let principal = Uuid::new_v4().to_string();
            let target = Uuid::new_v4().to_string();
            let stream_id = Uuid::new_v4().to_string();
            trace_principal::ActiveModel {
                id: Set(principal.clone()),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(&db)
            .await
            .expect("seed mtr13 trace principal");
            trace_target::ActiveModel {
                id: Set(target.clone()),
                target_kind: Set("import".to_owned()),
                project_path: Set("/third-party/mtr13-iso".to_owned()),
                backing_import_repo_id: Set(None),
                active: Set(true),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(&db)
            .await
            .expect("seed mtr13 trace target");
            trace_stream::ActiveModel {
                id: Set(stream_id),
                principal_id: Set(principal),
                target_id: Set(target),
                source_repo_id: Set("mtr13-source".to_owned()),
                epoch_id: Set(Uuid::new_v4().to_string()),
                hash_kind: Set(format.to_owned()),
                tip_oid: Set(Some(c_trace.clone())),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(&db)
            .await
            .expect("seed mtr13 populated trace stream");

            let forbidden_trace_and_deleted = [
                c_trace.as_str(),
                t_trace.as_str(),
                b_trace.as_str(),
                v_trace_tag_oid.as_str(),
                v_deleted_oid.as_str(),
            ];
            let forbidden_single_branch = [
                c_trace.as_str(),
                t_trace.as_str(),
                b_trace.as_str(),
                v_trace_tag_oid.as_str(),
                v_deleted_oid.as_str(),
                c_side.as_str(),
                t_side.as_str(),
                b_side.as_str(),
                v_outside_oid.as_str(),
            ];

            for transport in ["http", "ssh"] {
                if transport == "ssh" {
                    fixture.start_ssh(true, false).await;
                }
                let remote_url = if transport == "http" {
                    git_cli::mega2_http_url(fixture.port, repo_path)
                } else {
                    fixture.ssh_url(repo_path)
                };
                let run_git = |fixture: &Fixture, args: &[&str]| {
                    let dir = if args.contains(&"fetch") {
                        let at = args.iter().position(|arg| *arg == "-C").expect("fetch directory");
                        args[at + 1]
                    } else {
                        *args.last().expect("clone directory")
                    };
                    let before = if args.contains(&"fetch") {
                        let root = fixture.shared.path().join(dir);
                        assert!(root.join(".git").is_dir(), "incremental client must already exist");
                        let odb = fixture.git_text(&[
                            "-C", dir, "cat-file", "--batch-all-objects", "--batch-check=%(objectname)",
                        ]).lines().map(str::to_owned).collect();
                        ClientBefore { pack_fingerprints: pack_fingerprints(&root, false), odb }
                    } else {
                        assert!(!fixture.shared.path().join(dir).exists(), "clone client must be new");
                        ClientBefore::default()
                    };
                    assert!(client_before.borrow_mut().insert(dir.to_owned(), before.clone()).is_none());
                    let packet_path = fixture.shared.path().join(format!("{dir}.packet"));
                    let wire_path = fixture.shared.path().join(format!("{dir}.wire-pack"));
                    let work_root = git_cli::git_cli_workdir().canonicalize().expect("shared work root");
                    let case_root = fixture.shared.path().canonicalize().expect("case root");
                    let relative = case_root.strip_prefix(&work_root).expect("case inside shared work root");
                    let (packet_runner_path, wire_runner_path) =
                        if git_cli::mega2_reachable_host() == git_cli::HOST_LOOPBACK {
                            (
                                packet_path.to_string_lossy().into_owned(),
                                wire_path.to_string_lossy().into_owned(),
                            )
                        } else {
                            (
                                format!("/work/{}/{dir}.packet", relative.display()),
                                format!("/work/{}/{dir}.wire-pack", relative.display()),
                            )
                        };
                    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
                    let alias = format!(
                        "alias.mtr13-wire=!GIT_TRACE_PACKET={} GIT_TRACE_PACKFILE={} GIT_NO_LAZY_FETCH=1 git",
                        quote(&packet_runner_path), quote(&wire_runner_path)
                    );
                    let mut traced_args = vec!["-c", alias.as_str(), "mtr13-wire"];
                    traced_args.extend_from_slice(args);
                    let out = if transport == "http" {
                        git_cli::git_cli_no_auth(fixture.shared.path(), &traced_args)
                    } else {
                        fixture.ssh_git(&traced_args)
                    };
                    assert!(
                        out.status.success(),
                        "MTR13_WIRE {format} {transport} {args:?} failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    let packet = fs::read_to_string(&packet_path).expect("read packet negotiation");
                    let sent: Vec<_> = packet.lines().filter_map(|line| line.split_once("> ").map(|(_, payload)| payload)).collect();
                    let version_two = packet.lines().any(|line| line.split_once("< ").is_some_and(|(_, payload)| payload == "version 2"));
                    let want_count = sent.iter().filter(|payload| payload.starts_with("want ")).count();
                    let have_oids: BTreeSet<String> = sent.iter().filter_map(|payload| {
                        payload.strip_prefix("have ").map(|oid| oid.split_whitespace().next().expect("have OID").to_owned())
                    }).collect();
                    if args.contains(&"fetch") {
                        assert_eq!(have_oids, BTreeSet::from([c1.clone()]), "MTR13_ACTUAL_HAVE {dir} incremental request must name the c1 baseline commit");
                        let mut have_odb = c1_objs.clone();
                        if !args.contains(&"--no-tags") {
                            have_odb.insert(v1_parent_oid.clone());
                        }
                        assert_eq!(before.odb, have_odb, "MTR13_ACTUAL_HAVE {dir} pre-request ODB must equal the known have closure and original tag objects");
                    } else {
                        assert!(have_oids.is_empty(), "MTR13_ACTUAL_HAVE {dir} fresh clone must not negotiate existing objects");
                    }
                    assert!(want_count > 0, "MTR13_WIRE {dir} must record actual wants");
                    let requested_v2 = args.windows(2).any(|pair| pair == ["-c", "protocol.version=2"]);
                    assert_eq!(version_two, requested_v2, "MTR13_WIRE {dir} negotiated protocol differs from requested protocol");
                    if requested_v2 {
                        assert!(sent.contains(&"command=fetch"), "MTR13_WIRE {dir} lacks v2 fetch command");
                    }
                    let include_tag = sent.iter().any(|payload| payload.split_whitespace().any(|word| word == "include-tag"));
                    if args.contains(&"--no-tags") {
                        assert!(!include_tag, "MTR13_WIRE {dir} --no-tags requested include-tag");
                    } else if args.contains(&"--depth=1") || args.contains(&"fetch") || (requested_v2 && args.contains(&"--filter=blob:none")) {
                        assert!(include_tag, "MTR13_WIRE {dir} requires include-tag positive control");
                    }
                    if !requested_v2 && args.contains(&"--filter=blob:none") {
                        assert!(String::from_utf8_lossy(&out.stderr).contains("filtering not recognized"), "MTR13_WIRE {dir} v0 filter fallback must be explicit");
                    }
                    assert!(wire_packs.borrow_mut().insert(dir.to_owned(), wire_path).is_none());
                    eprintln!("MTR13_WIRE_OK format={format} case={dir} negotiated_v2={version_two} wants={want_count} haves={} before_objects={} old_pack_files={} include_tag={include_tag}", have_oids.len(), before.odb.len(), before.pack_fingerprints.len() / 2);
                };

                for proto in ["0", "2"] {
                    let p_flag = format!("protocol.version={proto}");
                    let prefix = format!("mtr13-{format}-{transport}-v{proto}");

                    // 1. normal
                    let dir_normal = format!("{prefix}-normal");
                    run_git(
                        &fixture,
                        &[
                            "-c",
                            &p_flag,
                            "-c",
                            "fetch.unpackLimit=1",
                            "-c",
                            "transfer.unpackLimit=1",
                            "clone",
                            "--single-branch",
                            "--branch",
                            "main",
                            &remote_url,
                            &dir_normal,
                        ],
                    );
                    verify_pack_and_odb(
                        &fixture,
                        &dir_normal,
                        false,
                        &main_with_tags,
                        &main_with_tags,
                        &forbidden_single_branch,
                        true,
                        true,
                        true,
                    );

                    // 2. bare
                    let dir_bare = format!("{prefix}-bare");
                    run_git(
                        &fixture,
                        &[
                            "-c",
                            &p_flag,
                            "-c",
                            "fetch.unpackLimit=1",
                            "-c",
                            "transfer.unpackLimit=1",
                            "clone",
                            "--bare",
                            "--single-branch",
                            "--branch",
                            "main",
                            &remote_url,
                            &dir_bare,
                        ],
                    );
                    verify_pack_and_odb(
                        &fixture,
                        &dir_bare,
                        true,
                        &main_with_tags,
                        &main_with_tags,
                        &forbidden_single_branch,
                        true,
                        true,
                        true,
                    );

                    // 3. mirror
                    let dir_mirror = format!("{prefix}-mirror");
                    run_git(
                        &fixture,
                        &[
                            "-c",
                            &p_flag,
                            "-c",
                            "fetch.unpackLimit=1",
                            "-c",
                            "transfer.unpackLimit=1",
                            "clone",
                            "--mirror",
                            &remote_url,
                            &dir_mirror,
                        ],
                    );
                    verify_pack_and_odb(
                        &fixture,
                        &dir_mirror,
                        true,
                        &mirror_all_objs,
                        &mirror_all_objs,
                        &forbidden_trace_and_deleted,
                        true,
                        true,
                        true,
                    );

                    // 4. no-tags
                    let dir_notags = format!("{prefix}-no-tags");
                    run_git(
                        &fixture,
                        &[
                            "-c",
                            &p_flag,
                            "-c",
                            "fetch.unpackLimit=1",
                            "-c",
                            "transfer.unpackLimit=1",
                            "clone",
                            "--no-tags",
                            &remote_url,
                            &dir_notags,
                        ],
                    );
                    verify_pack_and_odb(
                        &fixture,
                        &dir_notags,
                        false,
                        &main_objs,
                        &main_objs,
                        &[
                            c_trace.as_str(),
                            t_trace.as_str(),
                            b_trace.as_str(),
                            v_trace_tag_oid.as_str(),
                            v_deleted_oid.as_str(),
                            v1_parent_oid.as_str(),
                            v2_commit_oid.as_str(),
                            v2_inner_oid.as_str(),
                            v2_outer_oid.as_str(),
                            v2_tree_oid.as_str(),
                            v2_blob_oid.as_str(),
                            v_outside_oid.as_str(),
                        ],
                        true,
                        false,
                        false,
                    );

                    // 5. shallow (--depth=1)
                    let dir_shallow = format!("{prefix}-shallow");
                    run_git(
                        &fixture,
                        &[
                            "-c",
                            &p_flag,
                            "-c",
                            "fetch.unpackLimit=1",
                            "-c",
                            "transfer.unpackLimit=1",
                            "clone",
                            "--single-branch",
                            "--branch",
                            "main",
                            "--depth=1",
                            "--no-checkout",
                            &remote_url,
                            &dir_shallow,
                        ],
                    );
                    verify_pack_and_odb(
                        &fixture,
                        &dir_shallow,
                        false,
                        &shallow_with_tags,
                        &shallow_with_tags,
                        &[
                            c_trace.as_str(),
                            t_trace.as_str(),
                            b_trace.as_str(),
                            v_trace_tag_oid.as_str(),
                            v_deleted_oid.as_str(),
                            c1.as_str(),
                            t1.as_str(),
                            sub1.as_str(),
                            b_transient.as_str(),
                            v1_parent_oid.as_str(),
                            c_side.as_str(),
                            v_outside_oid.as_str(),
                        ],
                        true,
                        true,
                        true,
                    );

                    // 6. blob:none (--filter=blob:none)
                    let dir_blobless = format!("{prefix}-blobless");
                    run_git(
                        &fixture,
                        &[
                            "-c",
                            &p_flag,
                            "-c",
                            "fetch.unpackLimit=1",
                            "-c",
                            "transfer.unpackLimit=1",
                            "clone",
                            "--single-branch",
                            "--branch",
                            "main",
                            "--filter=blob:none",
                            "--no-checkout",
                            &remote_url,
                            &dir_blobless,
                        ],
                    );
                    if proto == "2" {
                        verify_pack_and_odb(
                            &fixture,
                            &dir_blobless,
                            false,
                            &blobless_v2_with_tags,
                            &blobless_v2_with_tags,
                            &[
                                c_trace.as_str(),
                                t_trace.as_str(),
                                b_trace.as_str(),
                                v_trace_tag_oid.as_str(),
                                v_deleted_oid.as_str(),
                                b_transient.as_str(),
                                b_shared.as_str(),
                                b_keep.as_str(),
                                b_readme.as_str(),
                                v2_blob_oid.as_str(),
                                c_side.as_str(),
                                v_outside_oid.as_str(),
                            ],
                            false,
                            true,
                            false,
                        );
                    } else {
                        verify_pack_and_odb(
                            &fixture,
                            &dir_blobless,
                            false,
                            &main_with_tags,
                            &main_with_tags,
                            &forbidden_single_branch,
                            false,
                            true,
                            true,
                        );
                    }

                    // 7a. incremental fetch from c1 -> c2 WITH include-tag (default fetch)
                    let dir_inc = format!("{prefix}-incremental");
                    fixture.git(&[
                        "-c",
                        "fetch.unpackLimit=999999",
                        "-c",
                        "transfer.unpackLimit=999999",
                        "clone",
                        "--no-local",
                        "inc-base",
                        &dir_inc,
                    ]);
                    run_git(
                        &fixture,
                        &[
                            "-C",
                            &dir_inc,
                            "-c",
                            &p_flag,
                            "-c",
                            "fetch.unpackLimit=1",
                            "-c",
                            "transfer.unpackLimit=1",
                            "fetch",
                            &remote_url,
                            "refs/heads/main:refs/remotes/origin/main",
                        ],
                    );
                    verify_pack_and_odb(
                        &fixture,
                        &dir_inc,
                        false,
                        &inc_with_tags,
                        &main_with_tags,
                        &forbidden_single_branch,
                        true,
                        true,
                        true,
                    );

                    // 7b. incremental fetch from c1 -> c2 WITH --no-tags (include-tag=false)
                    let dir_inc_notags = format!("{prefix}-incremental-notags");
                    fixture.git(&[
                        "-c",
                        "fetch.unpackLimit=999999",
                        "-c",
                        "transfer.unpackLimit=999999",
                        "clone",
                        "--no-local",
                        "--no-tags",
                        "inc-base-notags",
                        &dir_inc_notags,
                    ]);
                    run_git(
                        &fixture,
                        &[
                            "-C",
                            &dir_inc_notags,
                            "-c",
                            &p_flag,
                            "-c",
                            "fetch.unpackLimit=1",
                            "-c",
                            "transfer.unpackLimit=1",
                            "fetch",
                            "--no-tags",
                            &remote_url,
                            "refs/heads/main:refs/remotes/origin/main",
                        ],
                    );
                    verify_pack_and_odb(
                        &fixture,
                        &dir_inc_notags,
                        false,
                        &c2_only_objs,
                        &main_objs,
                        &forbidden_single_branch,
                        true,
                        false,
                        false,
                    );
                    eprintln!(
                        "MTR13_MATRIX_OK format={format} transport={transport} proto=v{proto} modes=8"
                    );
                }
            }
        })
        .catch_unwind()
        .await;
        fixture.stop();
        let closed = db.close().await;
        drop(guard);
        if let Err(panic) = result {
            let logs = fixture.temp.keep();
            eprintln!(
                "code-clone exact matrix ({format}) fixture logs: {}",
                logs.display()
            );
            std::panic::resume_unwind(panic);
        }
        closed.expect("close exact matrix pool");
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
                let protocol = if v2 { "v2" } else { "v0" };
                assert_eq!(response.status, 200, "authorized code have remains usable in {protocol}");
                assert!(response.body.windows(expected_ack.len()).any(|window| window == expected_ack.as_bytes()), "{protocol} must acknowledge the authorized code have");
                assert!(response.body.windows(4).any(|window| window == b"PACK"), "{protocol} must return a PACK even when want equals have");
                let pack = if v2 {
                    let mut acknowledgments = Vec::new();
                    pkt(&mut acknowledgments, "acknowledgments\n");
                    pkt(&mut acknowledgments, &format!("{expected_ack}\n"));
                    pkt(&mut acknowledgments, "ready\n");
                    acknowledgments.extend_from_slice(b"0001");
                    assert!(response.body.starts_with(&acknowledgments), "{protocol} negotiation without done must acknowledge the common have and become ready");
                    http_git_unpack_response(&response.body[acknowledgments.len()..], true)
                } else {
                    let mut remaining = response.body.as_slice();
                    let mut ack_count = 0;
                    while !remaining.starts_with(b"PACK") {
                        assert!(remaining.len() >= 4, "{protocol} truncated acknowledgment header");
                        let length = usize::from_str_radix(std::str::from_utf8(&remaining[..4]).expect("v0 acknowledgment header"), 16).expect("v0 acknowledgment length");
                        assert!((5..=65520).contains(&length) && length <= remaining.len(), "{protocol} invalid acknowledgment packet length {length}");
                        let acknowledgment = std::str::from_utf8(&remaining[4..length]).expect("v0 acknowledgment text").trim_end();
                        assert!(acknowledgment == expected_ack || acknowledgment == format!("{expected_ack} common"), "{protocol} unexpected acknowledgment {acknowledgment:?}");
                        ack_count += 1;
                        remaining = &remaining[length..];
                    }
                    assert!(ack_count > 0, "{protocol} must precede its raw PACK with an ACK");
                    remaining
                        .strip_suffix(b"0000")
                        .expect("v0 raw PACK must end with the transport flush")
                        .to_vec()
                };
                let checksum_bytes = match fixture.object_format.as_str() {
                    "sha1" => 20,
                    "sha256" | "blake3" => 32,
                    other => panic!("unsupported fixture object format {other}"),
                };
                assert_eq!(pack.len(), 12 + checksum_bytes, "{protocol} empty PACK must contain only its header and configured hash trailer");
                assert_eq!(&pack[..8], b"PACK\0\0\0\x02", "{protocol} empty PACK magic/version");
                assert_eq!(u32::from_be_bytes(pack[8..12].try_into().expect("PACK count")), 0, "{protocol} want-minus-have must contain zero objects");
                let checksum = match fixture.object_format.as_str() {
                    "sha1" => sha1::Sha1::digest(&pack[..12]).to_vec(),
                    "sha256" => Sha256::digest(&pack[..12]).to_vec(),
                    "blake3" => blake3::hash(&pack[..12]).as_bytes().to_vec(),
                    _ => unreachable!("object format validated above"),
                };
                assert_eq!(&pack[12..], checksum.as_slice(), "{protocol} empty PACK independent configured-hash checksum");
                eprintln!("MTR13_EMPTY_PACK protocol={protocol} object_format={} bytes={} count=0 checksum={}", fixture.object_format, pack.len(), hex::encode(checksum));
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
async fn code_clone_isolation_code_path_objects() {
    disabled_http::code_clone_isolation_code_path_objects().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_clone_isolation_exact_pack_matrix() {
    disabled_http::code_clone_isolation_exact_pack_matrix().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_clone_isolation_capacity_timeout_and_race() {
    disabled_http::code_clone_ref_budget_answers_413().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_clone_ref_budget_answers_413() {
    disabled_http::code_clone_ref_budget_answers_413().await;
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
async fn discovery_wire_http_v0_metadata() {
    disabled_http::http_v0_metadata_discovery_probe().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_wire_http_sha1() {
    disabled_http::run_metadata_wire_matrix("sha1", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_wire_http_sha256() {
    disabled_http::run_metadata_wire_matrix("sha256", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scoped_token_key_matrix() {
    disabled_http::run_ssh_scoped_token_key_matrix().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scoped_token_key_roundtrip() {
    disabled_http::run_ssh_scoped_token_key_roundtrip().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projection_worker_projects_real_push_and_resumes() {
    disabled_http::run_projection_worker().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_git_sha1_roundtrip() {
    disabled_http::ssh_git_roundtrip("sha1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_git_sha256_roundtrip() {
    disabled_http::ssh_git_roundtrip("sha256").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_wire_ssh_sha1() {
    disabled_http::run_metadata_wire_matrix("sha1", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_wire_ssh_sha256() {
    disabled_http::run_metadata_wire_matrix("sha256", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scope_auth_reserved_login_rejects_none() {
    disabled_http::reserved_login_rejects_none().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scope_auth_ordinary_anonymous_cannot_exec_traces() {
    disabled_http::scope_auth(disabled_http::ScopeAuthCase::Anonymous).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scope_auth_static_namespace_owner_and_paths() {
    disabled_http::scope_auth(disabled_http::ScopeAuthCase::Static).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scope_auth_mono_rowid_owner_and_project_write() {
    disabled_http::scope_auth(disabled_http::ScopeAuthCase::Mono).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scope_auth_deleted_key_cannot_exec_on_authenticated_session() {
    disabled_http::scope_auth(disabled_http::ScopeAuthCase::Deleted).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scope_auth_replaced_same_key_id_cannot_reuse_authenticated_session() {
    disabled_http::scope_auth(disabled_http::ScopeAuthCase::Replaced).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scope_auth_ambiguous_fingerprint_rejects_same_and_distinct_accounts() {
    disabled_http::scope_auth(disabled_http::ScopeAuthCase::Ambiguous).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_scope_auth_requires_verified_key_not_offered_key() {
    disabled_http::scope_auth(disabled_http::ScopeAuthCase::VerifiedKey).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_disabled_ssh_upload() {
    disabled_http::run_ssh(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_disabled_ssh_receive() {
    disabled_http::run_ssh(true).await;
}
