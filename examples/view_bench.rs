//! Opt-in history projection benchmark. Run only with a frozen params file.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{BufReader, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::{
        object::{
            ObjectTrait,
            blob::Blob,
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
            types::ObjectType,
        },
        pack::Pack,
    },
};
use mega2_core::{
    mega_refs, mega_view_filter,
    view_bench_ops::{
        BenchHarness, Filter, REGISTER_SCALE_LIMITS, RootChainOutcome, database_connection,
        db_config, generate_id, parse_for_registration, sort_git_tree_items,
        validate_for_registration,
    },
};
use regex::Regex;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend, Set, Statement,
    TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use url::Url;

const OUTPUT_DIR: &str = "target/tmp/view_bench";
const WORK_DIR: &str = "target/tmp/view_bench/work";
const CLASSES: [&str; 3] = ["in_view", "outside", "t_only"];
const VIEWS: [&str; 2] = [":/Documentation", ":exclude[::t/]"];
const FAILURE_MARKER: &str = "view_bench: injected failure after-migrate (schema created)";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Params {
    data: DataParams,
    views_config: ViewsConfigParams,
    views: Vec<String>,
    probes: Vec<ProbeClasses>,
    runs: RunParams,
    thresholds: Thresholds,
    scale_limits: ScaleParams,
    env: EnvParams,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DataParams {
    tag: String,
    commit: String,
    format: String,
    path: String,
    sha256: String,
    h_max: usize,
    git_version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ViewsConfigParams {
    batch_size: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Probe {
    path: String,
    content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProbeClasses {
    in_view: Vec<Probe>,
    outside: Vec<Probe>,
    t_only: Vec<Probe>,
}

impl ProbeClasses {
    fn get(&self, class: &str) -> &[Probe] {
        match class {
            "in_view" => &self.in_view,
            "outside" => &self.outside,
            "t_only" => &self.t_only,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RunParams {
    w: usize,
    n: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Thresholds {
    k_h: f64,
    k_b: f64,
    c_cold: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ScaleCaps {
    compose_members: usize,
    exclude_selectors: usize,
    k: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ScaleParams {
    caps: ScaleCaps,
    filter: String,
    probe_paths: Vec<String>,
    content: String,
    p95_budget_us: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct EnvParams {
    cpu: String,
    cores: usize,
    memory: String,
    storage: String,
    os: String,
    postgres_image: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Sample {
    elapsed_us: u64,
    statements: u64,
}

#[derive(Clone)]
struct MetricCounter {
    active: Arc<AtomicBool>,
    statements: Arc<AtomicU64>,
}

impl MetricCounter {
    fn install(connection: &mut DatabaseConnection) -> Self {
        let counter = Self {
            active: Arc::new(AtomicBool::new(false)),
            statements: Arc::new(AtomicU64::new(0)),
        };
        let callback = counter.clone();
        connection.set_metric_callback(move |_| {
            if callback.active.load(Ordering::Relaxed) {
                callback.statements.fetch_add(1, Ordering::Relaxed);
            }
        });
        counter
    }

    async fn measure<T, F>(&self, future: F) -> Result<(T, Sample)>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        self.statements.store(0, Ordering::Relaxed);
        self.active.store(true, Ordering::Relaxed);
        let start = Instant::now();
        let result = future.await;
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.active.store(false, Ordering::Relaxed);
        let statements = self.statements.load(Ordering::Relaxed);
        Ok((
            result?,
            Sample {
                elapsed_us,
                statements,
            },
        ))
    }
}

fn main() {
    if std::env::set_current_dir(env!("CARGO_MANIFEST_DIR")).is_err() {
        record_failure("cannot enter manifest directory");
        std::process::exit(2);
    }
    std::panic::set_hook(Box::new(|_| eprintln!("view_bench: benchmark panic")));
    let args: Vec<String> = std::env::args().collect();
    let mut params_path = None;
    let mut inject_failure = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--params" if i + 1 < args.len() => {
                params_path = Some(args[i + 1].clone());
                i += 2;
            }
            "--inject-failure" if args.get(i + 1).is_some_and(|s| s == "after-migrate") => {
                inject_failure = true;
                i += 2;
            }
            _ => {
                record_failure("invalid command line");
                std::process::exit(2);
            }
        }
    }
    let result = params_path
        .as_deref()
        .context("missing --params")
        .and_then(|path| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("runtime")
                .and_then(|runtime| {
                    let path = path.to_owned();
                    runtime.block_on(async move {
                        tokio::spawn(async move { run(&path, inject_failure).await })
                            .await
                            .map_err(|_| anyhow::anyhow!("benchmark task panic"))?
                    })
                })
        });
    if let Err(error) = result {
        let reason = redact_error(&error);
        eprintln!("view_bench: {reason}");
        record_failure(&reason);
        std::process::exit(2);
    }
}

fn redact_error(error: &anyhow::Error) -> String {
    let mut reason = format!("{error:#}");
    if let Ok(secret) = std::env::var("MEGA_DATABASE__DB_URL") {
        reason = reason.replace(&secret, "[redacted database URL]");
        if let Ok(url) = Url::parse(&secret)
            && let Some(password) = url.password()
        {
            reason = reason.replace(password, "[redacted password]");
        }
    }
    if let Ok(url_pattern) = Regex::new(r#"(?i)postgres(?:ql)?://[^\s'"<>]+"#) {
        reason = url_pattern
            .replace_all(&reason, "[redacted database URL]")
            .into_owned();
    }
    reason
}

fn record_failure(reason: &str) {
    let _ = fs::create_dir_all(OUTPUT_DIR);
    let bytes = json!({"exit_code": 2, "reason": reason}).to_string();
    loop {
        let path = format!(
            "{OUTPUT_DIR}/failed-{}.json",
            Utc::now().format("%Y%m%dT%H%M%SZ")
        );
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                let _ = file.write_all(bytes.as_bytes());
                break;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(_) => break,
        }
    }
}

async fn run(params_path: &str, inject_failure: bool) -> Result<()> {
    let params_bytes = fs::read(params_path).context("read params")?;
    ensure!(
        !params_bytes.windows(3).any(|w| w == b"TBD"),
        "params contain TBD"
    );
    let params_json: Json = serde_json::from_slice(&params_bytes).context("parse params JSON")?;
    let params: Params = serde_json::from_value(params_json.clone()).context("params shape")?;
    validate_params(&params, params_path)?;
    let code = code_version()?;
    verify_pack(&params.data)?;
    let admin_url =
        std::env::var("MEGA_DATABASE__DB_URL").context("MEGA_DATABASE__DB_URL is required")?;
    let admin = Database::connect(&admin_url)
        .await
        .context("connect admin database")?;
    check_no_leftover_schemas(&admin).await?;
    let (source_commits, trees) = decode_pack(&params.data)?;
    let chain = linearize_first_parent(&params.data, &source_commits)?;

    let tiers = [
        params.data.h_max / 4,
        params.data.h_max / 2,
        params.data.h_max,
    ];
    let mut cold_start = Vec::new();
    let mut incremental = Vec::new();
    let mut scale_limits = Json::Null;
    let mut server_version = None;
    for (tier_index, h) in tiers.into_iter().enumerate() {
        let schema = format!("view_bench_{}_{}", std::process::id(), tier_index + 1);
        admin
            .execute_unprepared(&format!("CREATE SCHEMA {schema}"))
            .await
            .context("create benchmark schema")?;
        let params_copy = params.clone();
        let chain_copy = chain.clone();
        let trees_copy = trees.clone();
        let schema_copy = schema.clone();
        let admin_url_copy = admin_url.clone();
        let task = tokio::spawn(async move {
            run_tier(
                &params_copy,
                h,
                &schema_copy,
                &admin_url_copy,
                &chain_copy,
                &trees_copy,
                inject_failure && tier_index == 0,
            )
            .await
        });
        let tier_result = task
            .await
            .map_err(|_| anyhow::anyhow!("benchmark task panic"));
        let cleanup_result = cleanup_schema(&admin, &schema).await;
        cleanup_result.context("cleanup benchmark schema")?;
        let (cold, groups, scale, version) = tier_result??;
        server_version.get_or_insert(version);
        cold_start.push(cold);
        incremental.extend(groups);
        if h == params.data.h_max {
            scale_limits = scale;
        }
    }
    admin.close().await?;

    let criteria = evaluate_criteria(&params, &cold_start, &incremental)?;
    let evidence = json!({
        "params": params_json,
        "env": {
            "cpu": params.env.cpu,
            "cores": params.env.cores,
            "memory": params.env.memory,
            "storage": params.env.storage,
            "os": params.env.os,
            "postgres_image": params.env.postgres_image,
            "server_version": server_version.context("server version missing")?,
            "params_path": params_path,
            "code": code,
        },
        "cold_start": cold_start,
        "incremental": incremental,
        "criteria": criteria,
        "scale_limits": scale_limits,
    });
    fs::create_dir_all(OUTPUT_DIR)?;
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    let copy = format!(
        "{OUTPUT_DIR}/run-{}.json",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    fs::write(&copy, &bytes)?;
    fs::write(format!("{OUTPUT_DIR}/latest.json"), &bytes)?;
    for key in ["a", "b", "c", "d"] {
        let verdict = if evidence["criteria"][key]["pass"] == true {
            "PASS"
        } else {
            "FAIL"
        };
        println!("criterion {key}: {verdict}");
    }
    println!(
        "scale limit: {} (p95 {} us)",
        if evidence["scale_limits"]["p95_us"]
            .as_u64()
            .unwrap_or(u64::MAX)
            <= params.scale_limits.p95_budget_us
        {
            "PASS"
        } else {
            "FAIL"
        },
        evidence["scale_limits"]["p95_us"]
    );
    println!("evidence: {copy}");
    println!(
        "summary: H_max={} groups={} scale_p95_us={}",
        params.data.h_max,
        evidence["incremental"].as_array().map_or(0, Vec::len),
        evidence["scale_limits"]["p95_us"]
    );
    Ok(())
}

fn validate_params(params: &Params, params_path: &str) -> Result<()> {
    ensure!(
        params_path == "examples/view_bench.params.json",
        "params path is not frozen"
    );
    ensure!(params.data.format == "pack", "data.format must be pack");
    ensure!(params.data.tag.starts_with('v'), "data tag is missing");
    ensure!(params.data.commit.len() == 40, "data commit must be SHA-1");
    ensure!(params.data.h_max >= 4, "h_max is too small");
    ensure!(
        params.data.git_version.starts_with("git version "),
        "git version missing"
    );
    ensure!(
        params.data.path.starts_with("target/tmp/view_bench/data/")
            && !params.data.path.split('/').any(|part| part == "..")
            && Path::new(&params.data.path).is_relative(),
        "data.path must be inside the ignored benchmark data directory"
    );
    ensure!(
        params.views_config.batch_size == 1000,
        "batch_size must be 1000"
    );
    ensure!(
        params.views == VIEWS,
        "views differ from the frozen protocol"
    );
    ensure!(
        params.probes.len() == params.views.len(),
        "probe/view count mismatch"
    );
    ensure!(
        params.runs.w == 5 && params.runs.n == 50,
        "run counts differ from protocol"
    );
    ensure!(
        params.thresholds.k_h == 1.5
            && params.thresholds.k_b == 1.0
            && params.thresholds.c_cold == 50,
        "thresholds differ from protocol"
    );
    let mut unique = HashSet::new();
    let mut outside_paths = HashSet::new();
    let mut t_only_paths = HashSet::new();
    for view_probes in &params.probes {
        outside_paths.extend(view_probes.outside.iter().map(|probe| &probe.path));
        t_only_paths.extend(view_probes.t_only.iter().map(|probe| &probe.path));
        for class in CLASSES {
            let entries = view_probes.get(class);
            ensure!(
                entries.len() == params.runs.w + params.runs.n,
                "probe count mismatch"
            );
            for probe in entries {
                let valid_path = match class {
                    "in_view" => probe.path.starts_with("Documentation/"),
                    "outside" | "t_only" => probe.path.starts_with("t/"),
                    _ => false,
                };
                ensure!(valid_path, "probe path outside its class");
                ensure!(
                    unique.insert((probe.path.clone(), probe.content.clone())),
                    "duplicate probe pair"
                );
            }
        }
    }
    ensure!(
        outside_paths.is_disjoint(&t_only_paths),
        "outside and t_only share a file"
    );
    let caps = &params.scale_limits.caps;
    ensure!(
        caps.compose_members == REGISTER_SCALE_LIMITS.members
            && caps.exclude_selectors == REGISTER_SCALE_LIMITS.selectors
            && caps.k == REGISTER_SCALE_LIMITS.k,
        "scale caps differ from registration limits"
    );
    ensure!(
        params.scale_limits.probe_paths.len() == caps.k,
        "scale probe path count"
    );
    ensure!(
        params.scale_limits.content.contains("{i}"),
        "scale content template"
    );
    ensure!(
        params.scale_limits.p95_budget_us == 1_000_000,
        "scale p95 budget"
    );
    let config = mega2_core::config::testing::isolated_config(WORK_DIR);
    for view in &params.views {
        let parsed = parse_for_registration(view)?;
        validate_for_registration(&parsed.filter, &config.monorepo)?;
    }
    let scale = parse_for_registration(&params.scale_limits.filter)?;
    let registration = validate_for_registration(&scale.filter, &config.monorepo)?;
    let (members, selectors) = count_filter_scale(&scale.filter);
    ensure!(
        members == caps.compose_members
            && selectors == caps.exclude_selectors
            && registration.src_paths.len() == caps.k,
        "scale filter normalized counts differ from caps"
    );
    let unique_paths: HashSet<_> = params.scale_limits.probe_paths.iter().collect();
    ensure!(unique_paths.len() == caps.k, "duplicate scale probe paths");
    for (path, src) in params
        .scale_limits
        .probe_paths
        .iter()
        .zip(&registration.src_paths)
    {
        let prefix = format!("{}/", src.trim_start_matches('/'));
        ensure!(path.starts_with(&prefix), "scale probe not under source");
    }
    ensure!(params.env.cores > 0, "environment cores missing");
    for text in [
        &params.env.cpu,
        &params.env.memory,
        &params.env.storage,
        &params.env.os,
        &params.env.postgres_image,
    ] {
        ensure!(!text.trim().is_empty(), "environment field is empty");
    }
    Ok(())
}

fn count_filter_scale(filter: &Filter) -> (usize, usize) {
    match filter {
        Filter::Exclude(selectors) => (0, selectors.len()),
        Filter::Compose(members) => members.iter().fold((members.len(), 0), |sum, member| {
            let nested = count_filter_scale(member);
            (sum.0 + nested.0, sum.1 + nested.1)
        }),
        Filter::Chain(ops) => ops.iter().fold((0, 0), |sum, op| {
            let nested = count_filter_scale(op);
            (sum.0 + nested.0, sum.1 + nested.1)
        }),
        _ => (0, 0),
    }
}

fn code_version() -> Result<Json> {
    let revision = command_output("libra", &["rev-parse", "HEAD"])?;
    ensure!(
        revision.len() == 40 || revision.len() == 64,
        "invalid HEAD revision"
    );
    let status = command_output(
        "libra",
        &[
            "status",
            "--short",
            "src",
            "examples",
            "Cargo.toml",
            "Cargo.lock",
        ],
    )?;
    let worktree: Vec<_> = status.lines().map(str::to_owned).collect();
    ensure!(
        worktree.iter().all(|line| {
            line.len() >= 4 && matches!(&line[3..], "src/lib.rs" | "examples/view_bench.rs")
        }),
        "code version has changes outside the benchmark seam and example"
    );
    let mut sha256 = BTreeMap::new();
    for file in ["src/lib.rs", "examples/view_bench.rs"] {
        sha256.insert(file, sha256_file(Path::new(file))?);
    }
    Ok(json!({"revision": revision, "worktree": worktree, "sha256": sha256}))
}

fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .context("libra execution failed")?;
    ensure!(
        output.status.success(),
        "libra execution failed: {}",
        output.status
    );
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hasher.update(&buffer[..size]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn verify_pack(data: &DataParams) -> Result<()> {
    let path = Path::new(&data.path);
    ensure!(sha256_file(path)? == data.sha256, "data sha256 mismatch");
    let mut file = fs::File::open(path)?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)?;
    ensure!(&magic == b"PACK", "data is not a raw pack");
    Ok(())
}

type Decoded = (HashMap<String, Commit>, HashMap<String, Tree>);

fn decode_pack(data: &DataParams) -> Result<Decoded> {
    let commits = Arc::new(Mutex::new(HashMap::new()));
    let trees = Arc::new(Mutex::new(HashMap::new()));
    let parse_errors = Arc::new(Mutex::new(Vec::new()));
    let commits_sink = commits.clone();
    let trees_sink = trees.clone();
    let errors_sink = parse_errors.clone();
    let mut pack = Pack::new_with_hash_kind(
        HashKind::Sha1,
        Some(4),
        Some(512 * 1024 * 1024),
        Some(PathBuf::from(OUTPUT_DIR).join("pack-cache")),
        true,
    );
    let file = fs::File::open(&data.path)?;
    pack.decode(
        &mut BufReader::new(file),
        move |entry| {
            let id = entry.inner.hash.to_string();
            match entry.inner.obj_type {
                ObjectType::Commit => match Commit::from_bytes(&entry.inner.data, entry.inner.hash)
                {
                    Ok(commit) => {
                        commits_sink
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(id, commit);
                    }
                    Err(error) => {
                        errors_sink
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(format!("commit {id}: {error}"));
                    }
                },
                ObjectType::Tree => match Tree::from_bytes(&entry.inner.data, entry.inner.hash) {
                    Ok(tree) => {
                        trees_sink
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(id, tree);
                    }
                    Err(error) => {
                        errors_sink
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(format!("tree {id}: {error}"));
                    }
                },
                _ => {}
            }
        },
        None::<fn(ObjectHash)>,
    )
    .context("decode raw pack")?;
    let errors = parse_errors
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(first) = errors.first() {
        bail!(
            "pack object parse failures ({} total), first: {first}",
            errors.len()
        );
    }
    drop(errors);
    let commits = Arc::try_unwrap(commits)
        .map_err(|_| anyhow::anyhow!("commit decoder still referenced"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("commit decoder poisoned"))?;
    let trees = Arc::try_unwrap(trees)
        .map_err(|_| anyhow::anyhow!("tree decoder still referenced"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("tree decoder poisoned"))?;
    Ok((commits, trees))
}

fn linearize_first_parent(
    data: &DataParams,
    originals: &HashMap<String, Commit>,
) -> Result<Vec<Commit>> {
    let mut old_chain = Vec::new();
    let mut next = Some(data.commit.clone());
    while let Some(id) = next {
        let commit = originals
            .get(&id)
            .with_context(|| format!("first-parent commit missing: {id}"))?;
        old_chain.push(commit);
        next = commit.parent_commit_ids.first().map(ToString::to_string);
    }
    old_chain.reverse();
    ensure!(
        old_chain.len() == data.h_max,
        "first-parent chain length differs from h_max"
    );
    let mut result = Vec::with_capacity(old_chain.len());
    for original in old_chain {
        let parent = result
            .last()
            .map(|commit: &Commit| commit.id)
            .into_iter()
            .collect();
        result.push(Commit::new_with_kind(
            HashKind::Sha1,
            original.author.clone(),
            original.committer.clone(),
            original.tree_id,
            parent,
            &original.message,
        )?);
    }
    Ok(result)
}

async fn check_no_leftover_schemas(admin: &DatabaseConnection) -> Result<()> {
    let rows = admin
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT nspname FROM pg_namespace WHERE starts_with(nspname, 'view_bench_') ORDER BY nspname".to_owned(),
        ))
        .await?;
    if !rows.is_empty() {
        for row in rows {
            let schema: String = row.try_get("", "nspname")?;
            eprintln!("view_bench: leftover schema {schema}");
        }
        bail!("leftover benchmark schemas exist");
    }
    Ok(())
}

async fn cleanup_schema(admin: &DatabaseConnection, schema: &str) -> Result<()> {
    admin
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = $1 AND pid <> pg_backend_pid()",
            [Value::from(schema.to_owned())],
        ))
        .await?;
    admin
        .execute_unprepared(&format!(
            "SET lock_timeout = '120s'; DROP SCHEMA IF EXISTS {schema} CASCADE"
        ))
        .await?;
    Ok(())
}

fn schema_url(admin_url: &str, schema: &str) -> Result<String> {
    let mut url = Url::parse(admin_url)?;
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"))
        .append_pair("application_name", schema);
    Ok(url.to_string())
}

async fn run_tier(
    params: &Params,
    h: usize,
    schema: &str,
    admin_url: &str,
    chain: &[Commit],
    source_trees: &HashMap<String, Tree>,
    inject_failure: bool,
) -> Result<(Json, Vec<Json>, Json, String)> {
    let mut connection = database_connection(&db_config(schema_url(admin_url, schema)?))
        .await
        .context("migrate isolated benchmark schema")?;
    let server_version: String = connection
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SHOW server_version".to_owned(),
        ))
        .await?
        .context("server_version row")?
        .try_get("", "server_version")?;
    if inject_failure {
        eprintln!("{FAILURE_MARKER}");
        bail!("injected failure after-migrate");
    }
    let counter = MetricCounter::install(&mut connection);
    let connection = Arc::new(connection);
    let harness = BenchHarness::new(
        connection.clone(),
        params.views_config.batch_size,
        Path::new(WORK_DIR),
    )
    .await
    .context("assemble benchmark storage")?;
    let config = harness.config();
    ensure!(
        config.views.batch_size as usize == params.views_config.batch_size,
        "batch size mismatch"
    );
    let mut trees = source_trees.clone();
    persist_source_history(&harness, &connection, &chain[..h], &trees).await?;

    let ((), root_sample) = counter
        .measure(async {
            ensure!(
                harness
                    .extend_root_chain(params.views_config.batch_size)
                    .await?
                    == RootChainOutcome::CaughtUp,
                "root chain did not catch up"
            );
            Ok(())
        })
        .await?;
    let mut cold_views = Vec::new();
    let mut filter_pks = Vec::new();
    for view in &params.views {
        let filter_pk = insert_filter(&connection, &config, view).await?;
        filter_pks.push(filter_pk);
        let ((), sample) = counter
            .measure(async {
                ensure!(
                    harness.catch_up(filter_pk).await?,
                    "view did not reach ready"
                );
                Ok(())
            })
            .await?;
        cold_views.push(json!({
            "view": view,
            "elapsed_us": sample.elapsed_us,
            "statements": sample.statements,
            "batches": batches(h, params.views_config.batch_size),
            "commits_per_sec": h as f64 * 1_000_000.0 / sample.elapsed_us.max(1) as f64,
        }));
    }
    let relation_sizes = relation_sizes(&connection).await?;
    let cold = json!({
        "h": h,
        "root_chain": {
            "elapsed_us": root_sample.elapsed_us,
            "statements": root_sample.statements,
            "batches": batches(h, params.views_config.batch_size),
        },
        "views": cold_views,
        "relation_sizes": relation_sizes,
    });

    let mut current = chain[h - 1].clone();
    let mut groups: HashMap<(usize, String), (Vec<Sample>, Vec<Sample>)> = HashMap::new();
    for (class_index, class) in CLASSES.into_iter().enumerate() {
        for i in 0..params.runs.w + params.runs.n {
            for view_index in 0..params.views.len() {
                let probe = &params.probes[view_index].get(class)[i];
                current = append_probe(
                    &harness,
                    &connection,
                    &mut trees,
                    &current,
                    &[(probe.path.clone(), probe.content.clone())],
                    &format!("view benchmark h={h} class={class} i={i} v={view_index}"),
                )
                .await?;
                let ((), sample) = counter
                    .measure(async {
                        ensure!(
                            harness
                                .extend_root_chain(params.views_config.batch_size)
                                .await?
                                == RootChainOutcome::CaughtUp,
                            "incremental root chain did not catch up"
                        );
                        ensure!(
                            harness.catch_up(filter_pks[view_index]).await?,
                            "incremental view not ready"
                        );
                        Ok(())
                    })
                    .await?;
                for (other, filter_pk) in filter_pks.iter().enumerate() {
                    if other != view_index {
                        ensure!(harness.catch_up(*filter_pk).await?, "other view not ready");
                    }
                }
                let slot = groups
                    .entry((view_index, class.to_owned()))
                    .or_insert_with(|| (Vec::new(), Vec::new()));
                if i < params.runs.w {
                    slot.0.push(sample);
                } else {
                    slot.1.push(sample);
                }
            }
        }
        ensure!(class_index < 3, "invalid class index");
    }
    let mut incremental = Vec::new();
    for class in CLASSES {
        for (view_index, view) in params.views.iter().enumerate() {
            let (warmup, samples) = groups
                .remove(&(view_index, class.to_owned()))
                .context("sample group missing")?;
            incremental.push(json!({
                "h": h,
                "view": view,
                "probe_class": class,
                "warmup": warmup,
                "samples": samples,
                "p50_us": percentile(&samples, 50),
                "p95_us": percentile(&samples, 95),
            }));
        }
    }
    let scale = if h == params.data.h_max {
        run_scale_limits(
            params,
            &harness,
            &connection,
            &counter,
            &config,
            &mut trees,
            &mut current,
        )
        .await?
    } else {
        Json::Null
    };
    Ok((cold, incremental, scale, server_version))
}

async fn persist_source_history(
    harness: &BenchHarness,
    connection: &DatabaseConnection,
    commits: &[Commit],
    source_trees: &HashMap<String, Tree>,
) -> Result<()> {
    let mut needed = HashSet::new();
    for commit in commits {
        collect_tree_ids(commit.tree_id, source_trees, &mut needed)?;
    }
    let trees: Vec<_> = needed
        .iter()
        .map(|id| {
            source_trees
                .get(id)
                .cloned()
                .context("required tree missing")
        })
        .collect::<Result<_>>()?;
    for chunk in trees.chunks(1000) {
        harness.save_trees(chunk.to_vec(), commits[0].id).await?;
    }
    for chunk in commits.chunks(1000) {
        harness.save_commits(chunk.to_vec()).await?;
    }
    let first = &commits[0];
    let tip = commits.last().context("source history is empty")?;
    let now = Utc::now().naive_utc();
    let transaction = connection.begin().await?;
    harness
        .insert_root_ref(
            &transaction,
            mega_refs::Model {
                id: generate_id(),
                path: "/".to_owned(),
                ref_name: "refs/heads/main".to_owned(),
                ref_commit_hash: first.id.to_string(),
                ref_tree_hash: first.tree_id.to_string(),
                created_at: now,
                updated_at: now,
                is_cl: false,
            },
        )
        .await?;
    ensure!(
        harness
            .cas_root_ref(
                &transaction,
                &first.id.to_string(),
                &first.tree_id.to_string(),
                &tip.id.to_string(),
                &tip.tree_id.to_string(),
            )
            .await?,
        "seed root CAS missed"
    );
    transaction.commit().await?;
    Ok(())
}

fn collect_tree_ids(
    id: ObjectHash,
    trees: &HashMap<String, Tree>,
    needed: &mut HashSet<String>,
) -> Result<()> {
    let key = id.to_string();
    if !needed.insert(key.clone()) {
        return Ok(());
    }
    let tree = trees
        .get(&key)
        .with_context(|| format!("tree missing: {key}"))?;
    for item in &tree.tree_items {
        if item.is_tree() {
            collect_tree_ids(item.id, trees, needed)?;
        }
    }
    Ok(())
}

async fn insert_filter(
    connection: &DatabaseConnection,
    config: &mega2_core::config::Config,
    spec: &str,
) -> Result<i64> {
    let parsed = parse_for_registration(spec)?;
    let registration = validate_for_registration(&parsed.filter, &config.monorepo)?;
    let id = generate_id();
    let now = Utc::now().naive_utc();
    mega_view_filter::ActiveModel {
        id: Set(id),
        filter_id: Set(parsed.filter_id),
        canonical_spec: Set(parsed.canonical_text),
        algo_version: Set(1),
        object_format: Set(config.monorepo.object_format.as_str().to_owned()),
        src_paths: Set(json!(registration.src_paths)),
        push_enabled: Set(registration.push_enabled),
        projected_seq: Set(0),
        ready_seq: Set(None),
        warming_since: Set(Some(now)),
        last_access_at: Set(None),
        created_at: Set(now),
    }
    .insert(connection)
    .await?;
    Ok(id)
}

fn batches(length: usize, batch_size: usize) -> usize {
    length.div_ceil(batch_size)
}

fn percentile(samples: &[Sample], q: usize) -> u64 {
    let mut sorted: Vec<_> = samples.iter().map(|sample| sample.elapsed_us).collect();
    sorted.sort_unstable();
    let rank = (q * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

async fn relation_sizes(connection: &DatabaseConnection) -> Result<Json> {
    let mut sizes = BTreeMap::new();
    for name in [
        "mega_view_root_chain",
        "mega_view_commit_map",
        "mega_view_object",
        "mega_view_object_ref",
    ] {
        let row = connection
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT pg_total_relation_size($1::regclass) AS bytes",
                [Value::from(name.to_owned())],
            ))
            .await?
            .context("relation size row missing")?;
        sizes.insert(name, row.try_get::<i64>("", "bytes")?);
    }
    Ok(json!(sizes))
}

async fn append_probe(
    harness: &BenchHarness,
    connection: &DatabaseConnection,
    trees: &mut HashMap<String, Tree>,
    parent: &Commit,
    modifications: &[(String, String)],
    message: &str,
) -> Result<Commit> {
    let mut root = parent.tree_id;
    let mut new_trees = Vec::new();
    for (path, content) in modifications {
        let parts: Vec<_> = path.split('/').collect();
        ensure!(
            parts
                .iter()
                .all(|part| !part.is_empty() && *part != "." && *part != ".."),
            "invalid probe path"
        );
        root = update_tree(root, &parts, content.as_bytes(), trees, &mut new_trees)?;
        for tree in &new_trees {
            trees.insert(tree.id.to_string(), tree.clone());
        }
    }
    ensure!(
        root != parent.tree_id,
        "probe root tree equals parent root tree"
    );
    let commit = Commit::from_tree_id_with_kind(HashKind::Sha1, root, vec![parent.id], message)?;
    for tree in &new_trees {
        trees.insert(tree.id.to_string(), tree.clone());
    }
    harness.save_trees(new_trees, commit.id).await?;
    harness.save_commits(vec![commit.clone()]).await?;
    let transaction = connection.begin().await?;
    ensure!(
        harness
            .cas_root_ref(
                &transaction,
                &parent.id.to_string(),
                &parent.tree_id.to_string(),
                &commit.id.to_string(),
                &commit.tree_id.to_string(),
            )
            .await?,
        "probe root CAS missed"
    );
    transaction.commit().await?;
    Ok(commit)
}

async fn spine_tree_count(connection: &DatabaseConnection) -> Result<i64> {
    let row = connection
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*) AS n FROM mega_view_object WHERE kind = 2".to_owned(),
        ))
        .await?
        .context("spine tree count row missing")?;
    Ok(row.try_get("", "n")?)
}

async fn run_scale_limits(
    params: &Params,
    harness: &BenchHarness,
    connection: &DatabaseConnection,
    counter: &MetricCounter,
    config: &mega2_core::config::Config,
    trees: &mut HashMap<String, Tree>,
    current: &mut Commit,
) -> Result<Json> {
    let parsed = parse_for_registration(&params.scale_limits.filter)?;
    let registration = validate_for_registration(&parsed.filter, &config.monorepo)?;
    let (compose_members, exclude_selectors) = count_filter_scale(&parsed.filter);
    let k = registration.src_paths.len();
    let filter_pk = insert_filter(connection, config, &params.scale_limits.filter).await?;
    let before = spine_tree_count(connection).await?;
    let ((), cold) = counter
        .measure(async {
            ensure!(
                harness.catch_up(filter_pk).await?,
                "scale cold start not ready"
            );
            Ok(())
        })
        .await?;
    let cold_spines = spine_tree_count(connection).await? - before;
    let mut warmup = Vec::new();
    let mut samples = Vec::new();
    for i in 0..params.runs.w + params.runs.n {
        let changes: Vec<_> = params
            .scale_limits
            .probe_paths
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    params.scale_limits.content.replace("{i}", &i.to_string()),
                )
            })
            .collect();
        *current = append_probe(
            harness,
            connection,
            trees,
            current,
            &changes,
            &format!("view benchmark scale i={i}"),
        )
        .await?;
        let before = spine_tree_count(connection).await?;
        let ((), sample) = counter
            .measure(async {
                ensure!(
                    harness
                        .extend_root_chain(params.views_config.batch_size)
                        .await?
                        == RootChainOutcome::CaughtUp,
                    "scale root chain did not catch up"
                );
                ensure!(harness.catch_up(filter_pk).await?, "scale view not ready");
                Ok(())
            })
            .await?;
        let spine_trees = spine_tree_count(connection).await? - before;
        let item = json!({
            "elapsed_us": sample.elapsed_us,
            "statements": sample.statements,
            "spine_trees": spine_trees,
        });
        if i < params.runs.w {
            warmup.push(item);
        } else {
            samples.push(item);
        }
    }
    let p50_us = percentile_json(&samples, 50)?;
    let p95_us = percentile_json(&samples, 95)?;
    Ok(json!({
        "compose_members": compose_members,
        "exclude_selectors": exclude_selectors,
        "k": k,
        "cold_start": {
            "elapsed_us": cold.elapsed_us,
            "statements": cold.statements,
            "batches": batches(params.data.h_max + 6 * (params.runs.w + params.runs.n), params.views_config.batch_size),
            "spine_trees": cold_spines,
        },
        "warmup": warmup,
        "samples": samples,
        "p50_us": p50_us,
        "p95_us": p95_us,
    }))
}

fn percentile_json(samples: &[Json], q: usize) -> Result<u64> {
    let mut values: Vec<_> = samples
        .iter()
        .map(|sample| {
            sample["elapsed_us"]
                .as_u64()
                .context("sample elapsed_us missing")
        })
        .collect::<Result<_>>()?;
    ensure!(!values.is_empty(), "empty samples");
    values.sort_unstable();
    let rank = (q * values.len()).div_ceil(100);
    Ok(values[rank - 1])
}

fn evaluate_criteria(params: &Params, cold: &[Json], incremental: &[Json]) -> Result<Json> {
    let mut grouped: BTreeMap<(String, String), Vec<&Json>> = BTreeMap::new();
    for group in incremental {
        let view = group["view"].as_str().context("group view missing")?;
        let class = group["probe_class"]
            .as_str()
            .context("group probe class missing")?;
        grouped
            .entry((view.to_owned(), class.to_owned()))
            .or_default()
            .push(group);
    }
    let mut a_violations = Vec::new();
    let mut b_violations = Vec::new();
    let mut c_violations = Vec::new();
    let mut d_violations = Vec::new();
    let mut largest_ratio: f64 = 0.0;
    let mut largest_c_ratio: f64 = 0.0;
    let h_max = params.data.h_max as u64;
    let cold_max = cold
        .iter()
        .find(|item| item["h"].as_u64() == Some(h_max))
        .context("H_max cold result missing")?;
    let n_batches = batches(params.data.h_max, params.views_config.batch_size) as f64;
    for ((view, class), mut tiers) in grouped {
        tiers.sort_by_key(|group| group["h"].as_u64().unwrap_or(0));
        ensure!(
            tiers.len() == 3,
            "incremental tier count differs from three"
        );
        let series: Vec<Vec<u64>> = tiers
            .iter()
            .map(|group| {
                group["samples"]
                    .as_array()
                    .context("sample array missing")?
                    .iter()
                    .map(|sample| {
                        sample["statements"]
                            .as_u64()
                            .context("sample statements missing")
                    })
                    .collect::<Result<_>>()
            })
            .collect::<Result<_>>()?;
        if series[0] != series[1] || series[1] != series[2] {
            a_violations.push(format!("{view}/{class}"));
        }
        let p95: Vec<u64> = tiers
            .iter()
            .map(|group| group["p95_us"].as_u64().context("p95 missing"))
            .collect::<Result<_>>()?;
        let min = *p95.iter().min().context("p95 minimum missing")?;
        let max = *p95.iter().max().context("p95 maximum missing")?;
        let ratio = max as f64 / min.max(1) as f64;
        largest_ratio = largest_ratio.max(ratio);
        if ratio > params.thresholds.k_h {
            b_violations.push(format!("{view}/{class}: {ratio:.4}"));
        }
        let cold_view = cold_max["views"]
            .as_array()
            .context("cold views missing")?
            .iter()
            .find(|item| item["view"].as_str() == Some(view.as_str()))
            .context("cold view missing")?;
        let cold_per_batch = cold_view["elapsed_us"]
            .as_f64()
            .context("cold elapsed missing")?
            / n_batches;
        largest_c_ratio = largest_c_ratio.max(p95[2] as f64 / cold_per_batch.max(1.0));
        if p95[2] as f64 > params.thresholds.k_b * cold_per_batch {
            c_violations.push(format!("{view}/{class}"));
        }
    }
    let root_statements = cold_max["root_chain"]["statements"]
        .as_u64()
        .context("root statements missing")?;
    let allowed = params.thresholds.c_cold * n_batches as u64;
    let mut largest_cold_statements = 0_u64;
    for view in cold_max["views"].as_array().context("cold views missing")? {
        let statements = root_statements
            + view["statements"]
                .as_u64()
                .context("view statements missing")?;
        largest_cold_statements = largest_cold_statements.max(statements);
        if statements > allowed {
            d_violations.push(view["view"].as_str().unwrap_or("unknown").to_owned());
        }
    }
    Ok(json!({
        "a": {
            "value": a_violations.is_empty(),
            "threshold": true,
            "pass": a_violations.is_empty(),
            "violations": a_violations,
        },
        "b": {
            "value": largest_ratio,
            "threshold": params.thresholds.k_h,
            "pass": b_violations.is_empty(),
            "violations": b_violations,
        },
        "c": {
            "value": largest_c_ratio,
            "threshold": params.thresholds.k_b,
            "pass": c_violations.is_empty(),
            "violations": c_violations,
        },
        "d": {
            "value": largest_cold_statements,
            "threshold": allowed,
            "pass": d_violations.is_empty(),
            "violations": d_violations,
        },
    }))
}

fn update_tree(
    old_id: ObjectHash,
    path: &[&str],
    content: &[u8],
    trees: &HashMap<String, Tree>,
    new_trees: &mut Vec<Tree>,
) -> Result<ObjectHash> {
    let old = trees
        .get(&old_id.to_string())
        .context("probe parent tree missing")?;
    let mut items = old.tree_items.clone();
    let name = path[0];
    let new_item = if path.len() == 1 {
        let blob = Blob::from_content_bytes_with_kind(HashKind::Sha1, content.to_vec())?;
        TreeItem::new(TreeItemMode::Blob, blob.id, name.to_owned())
    } else {
        let child = items.iter().find(|item| item.name == name);
        let child_id = if let Some(child) = child {
            ensure!(child.is_tree(), "probe path crosses a file");
            update_tree(child.id, &path[1..], content, trees, new_trees)?
        } else {
            build_missing_tree(&path[1..], content, new_trees)?
        };
        TreeItem::new(TreeItemMode::Tree, child_id, name.to_owned())
    };
    items.retain(|item| item.name != name);
    items.push(new_item);
    sort_git_tree_items(&mut items);
    let tree = Tree::from_tree_items_with_kind(HashKind::Sha1, items)?;
    let id = tree.id;
    new_trees.push(tree);
    Ok(id)
}

fn build_missing_tree(
    path: &[&str],
    content: &[u8],
    new_trees: &mut Vec<Tree>,
) -> Result<ObjectHash> {
    let item = if path.len() == 1 {
        let blob = Blob::from_content_bytes_with_kind(HashKind::Sha1, content.to_vec())?;
        TreeItem::new(TreeItemMode::Blob, blob.id, path[0].to_owned())
    } else {
        let id = build_missing_tree(&path[1..], content, new_trees)?;
        TreeItem::new(TreeItemMode::Tree, id, path[0].to_owned())
    };
    let tree = Tree::from_tree_items_with_kind(HashKind::Sha1, vec![item])?;
    let id = tree.id;
    new_trees.push(tree);
    Ok(id)
}
