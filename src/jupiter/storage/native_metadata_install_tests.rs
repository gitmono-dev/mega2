use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use mst2_codec::metapage::{Entry, EntryKind};
use sea_orm::{Database, PaginatorTrait};
use sea_orm_migration::MigratorTrait;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, watch},
    task::{JoinHandle, JoinSet},
};

use super::*;
use crate::{
    ceres::snapshot::retention_dag::MetadataDagBuilder,
    jupiter::{
        migration::Migrator,
        tests::{TestSchemaGuard, test_db_config},
    },
};

fn prepared(scope: &str) -> PreparedNativeMetadataRetention {
    let child_entries = [Entry::file(EntryKind::Regular, b"file", 3, [42; 32])];
    let child = Page::build(&child_entries).unwrap();
    let entries = [
        Entry::dir(b"one", page_id(&child)),
        Entry::dir(b"two", page_id(&child)),
    ];
    let root = Page::build(&entries).unwrap();
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&child, &child_entries).unwrap();
    builder.add_directory(&root, &entries).unwrap();
    PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page_id(&root)).unwrap()),
        scope,
    )
}

async fn fixture() -> (
    DatabaseConnection,
    DatabaseConnection,
    TestSchemaGuard,
    String,
) {
    let temp = tempfile::TempDir::new().unwrap();
    let (config, schema) = test_db_config(temp.path()).await;
    let first = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&first, None).await.unwrap();
    let second = Database::connect(config.db_url.clone()).await.unwrap();
    (first, second, schema, config.db_url)
}

async fn install(
    repository: &PostgresMetadataInstallRepository,
    intent: &MetadataPrepareIntent,
    prepared: &PreparedNativeMetadataRetention,
) {
    for page in prepared.dag().payloads() {
        repository.install_page(intent, page).await.unwrap();
    }
}

fn rejected(error: MetadataInstallError) -> SnapshotErrorCode {
    match error {
        MetadataInstallError::Rejected(error) => error.code,
        _ => panic!("expected definite rejection"),
    }
}

#[tokio::test]
async fn native_metadata_two_primary_connections_replay_one_receipt_without_recounting() {
    let (first, second, _schema, _url) = fixture().await;
    let a = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let b = PostgresMetadataInstallRepository::new(second.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let (ia, ib) = tokio::join!(
        a.begin_intent("same", &prepared),
        b.begin_intent("same", &prepared)
    );
    let ia = ia.unwrap();
    assert_eq!(ia, ib.unwrap());
    for page in prepared.dag().payloads() {
        let (left, right) = tokio::join!(a.install_page(&ia, page), b.install_page(&ia, page));
        left.unwrap();
        right.unwrap();
    }
    let (ra, rb) = tokio::join!(a.finalize(&ia), b.finalize(&ia));
    assert_eq!(ra.unwrap(), rb.unwrap());
    assert_eq!(
        mst2_metadata_prepare::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        mst2_metadata_payload::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        mst2_retention_edge::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        2
    );
    let child = prepared.dag().edges()[0].child.clone();
    assert_eq!(
        PostgresRetentionRepository::new(first)
            .node(&child)
            .await
            .unwrap()
            .unwrap()
            .incoming_refs,
        1
    );
}

#[tokio::test]
async fn native_metadata_operation_conflict_and_multibyte_byte_limit_reject_without_writes() {
    let (first, second, _schema, _url) = fixture().await;
    let a = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let b = PostgresMetadataInstallRepository::new(second)
        .await
        .unwrap();
    a.begin_intent("bound", &prepared("/")).await.unwrap();
    assert_eq!(
        rejected(
            b.begin_intent("bound", &prepared("/scope"))
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::Conflict
    );
    assert_eq!(
        rejected(
            a.begin_intent(&"é".repeat(128), &prepared("/"))
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::InvalidRequest
    );
    assert_eq!(
        mst2_metadata_prepare::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        mst2_metadata_payload::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn native_metadata_protocol_hash_and_immutable_bytes_conflicts_cannot_be_overwritten() {
    let (first, _second, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository.begin_intent("bytes", &prepared).await.unwrap();
    let original = &prepared.dag().payloads()[0];
    let mut invalid = original.clone();
    invalid.id[0] ^= 1;
    assert_eq!(
        rejected(
            repository
                .install_page(&intent, &invalid)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::DigestMismatch
    );
    let mut oversized = original.clone();
    oversized.bytes.resize(PAGE_MAX_BYTES + 1, 0);
    oversized.size = oversized.bytes.len() as u64;
    oversized.id = page_id(&oversized.bytes);
    assert_eq!(
        rejected(
            repository
                .install_page(&intent, &oversized)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::LimitExceeded
    );
    // A corrupt existing row is never replaced by a good upload.
    let mut corrupt = original.bytes.clone();
    corrupt[0] ^= 1;
    first.execute_raw(statement("INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload) VALUES($1,1,$2,$3)",
        [original.id.to_vec().into(),(original.size as i32).into(),corrupt.clone().into()])).await.unwrap();
    assert_eq!(
        rejected(
            repository
                .install_page(&intent, original)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(
        mst2_metadata_payload::Entity::find_by_id(original.id.to_vec())
            .one(&first)
            .await
            .unwrap()
            .unwrap()
            .payload,
        corrupt
    );
    assert!(
        first
            .execute_unprepared("UPDATE mst2_metadata_payload SET payload=payload")
            .await
            .is_err()
    );
    assert!(
        first
            .execute_unprepared("DELETE FROM mst2_metadata_payload")
            .await
            .is_err()
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn native_metadata_partial_installation_survives_repository_and_connection_restart() {
    let (first, second, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository.begin_intent("restart", &prepared).await.unwrap();
    repository
        .install_page(&intent, &prepared.dag().payloads()[0])
        .await
        .unwrap();
    assert_eq!(
        mst2_retention_node::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        0
    );
    drop(repository);
    first.close().await.unwrap();
    let restarted = PostgresMetadataInstallRepository::new(second.clone())
        .await
        .unwrap();
    assert!(matches!(
        restarted
            .inspect_prepare(
                &second,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Payload
            )
            .await
            .unwrap(),
        MetadataPrepareObservation::Preparing(_)
    ));
    assert_eq!(
        restarted
            .load_installed_dag(&intent)
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::ObjectUnavailable
    );
    install(&restarted, &intent, &prepared).await;
    let receipt = restarted.finalize(&intent).await.unwrap();
    assert_eq!(receipt.metadata_root(), prepared.dag().root());
    assert_eq!(receipt.payload_bytes(), prepared.dag().payload_bytes());
}

#[tokio::test]
async fn native_metadata_half_installation_survives_real_process_kill() {
    const CHILD_DB: &str = "MEGA_MST2_METADATA_CRASH_CHILD_DB";
    const CHECKPOINT: &str = "MEGA_MST2_METADATA_CRASH_CHECKPOINT";
    const OPERATION: &str = "process-crash";
    if let Ok(url) = std::env::var(CHILD_DB) {
        let database = Database::connect(url).await.unwrap();
        let repository = PostgresMetadataInstallRepository::new(database)
            .await
            .unwrap();
        let prepared = prepared("/");
        let intent = repository.begin_intent(OPERATION, &prepared).await.unwrap();
        repository
            .install_page(&intent, &prepared.dag().payloads()[0])
            .await
            .unwrap();
        let checkpoint = std::path::PathBuf::from(std::env::var(CHECKPOINT).unwrap());
        let staging = checkpoint.with_extension("staging");
        std::fs::write(
            &staging,
            format!(
                "{}\n{}",
                intent.prepare_id(),
                hex::encode(intent.manifest_digest())
            ),
        )
        .unwrap();
        std::fs::rename(staging, checkpoint).unwrap();
        // Keep repository/connection alive until the parent kills this process.
        std::future::pending::<()>().await;
        unreachable!();
    }
    let (first, second, _schema, url) = fixture().await;
    let checkpoint_dir = tempfile::tempdir().unwrap();
    let checkpoint = checkpoint_dir.path().join("partial-installation");
    let full_name = concat!(
        module_path!(),
        "::native_metadata_half_installation_survives_real_process_kill"
    );
    let test_name = full_name.split_once("::").unwrap().1;
    let mut worker = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_DB, url)
        .env(CHECKPOINT, &checkpoint)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let marker = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if checkpoint.exists() {
                break std::fs::read_to_string(&checkpoint).unwrap();
            }
            if let Some(status) = worker.try_wait().unwrap() {
                panic!("metadata crash child exited before partial durable commit: {status}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("metadata crash child did not reach its durable checkpoint");
    worker.kill().await.unwrap();
    let status = worker.wait().await.unwrap();
    assert!(!status.success());
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
    first.close().await.unwrap();
    let restarted = PostgresMetadataInstallRepository::new(second.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let digest = prepared.install_plan().unwrap().digest().unwrap();
    let original = match restarted
        .inspect_prepare(&second, OPERATION, digest, MetadataCommitPhase::Payload)
        .await
        .unwrap()
    {
        MetadataPrepareObservation::Preparing(intent) => intent,
        other => panic!("half installation exposed a false committed receipt: {other:?}"),
    };
    let mut marker = marker.lines();
    assert_eq!(marker.next(), Some(original.prepare_id()));
    assert_eq!(marker.next(), Some(hex::encode(digest).as_str()));
    assert_eq!(
        restarted.begin_intent(OPERATION, &prepared).await.unwrap(),
        original
    );
    assert_eq!(
        mst2_metadata_payload::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        mst2_metadata_prepare_page::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        prepared.dag().payloads().len() as u64
    );
    assert_eq!(
        rejected(restarted.finalize(&original).await.unwrap_err()),
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_retention_node::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        0
    );
    install(&restarted, &original, &prepared).await;
    let receipt = restarted.finalize(&original).await.unwrap();
    assert_eq!(receipt.intent(), &original);
    assert_eq!(restarted.finalize(&original).await.unwrap(), receipt);
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        prepared.dag().payloads().len() as u64
    );
    assert_eq!(
        mst2_retention_edge::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        1
    );
}

async fn overwrite_installed_payload_for_test(
    connection: &DatabaseConnection,
    page: &MetadataPagePayload,
    bytes: Vec<u8>,
) {
    assert_eq!(bytes.len(), page.bytes.len());
    let txn = connection.begin().await.unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_payload DISABLE TRIGGER mst2_metadata_payload_immutable",
    )
    .await
    .unwrap();
    txn.execute_raw(statement(
        "UPDATE mst2_metadata_payload SET payload=$2 WHERE page_id=$1",
        [page.id.to_vec().into(), bytes.into()],
    ))
    .await
    .unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_payload ENABLE TRIGGER mst2_metadata_payload_immutable",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn native_metadata_committed_recovery_rejects_same_length_corruption_and_preserves_pins() {
    let (first, second, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository
        .begin_intent("corruption", &prepared)
        .await
        .unwrap();
    install(&repository, &intent, &prepared).await;
    let receipt = repository.finalize(&intent).await.unwrap();
    let page = &prepared.dag().payloads()[0];
    let counters = mst2_retention_node::Entity::find()
        .all(&first)
        .await
        .unwrap()
        .into_iter()
        .map(|node| (node.node_id, node.incoming_refs))
        .collect::<BTreeSet<_>>();
    let mut corrupt = page.bytes.clone();
    corrupt[0] ^= 1;
    overwrite_installed_payload_for_test(&first, page, corrupt).await;
    assert_eq!(
        rejected(
            repository
                .inspect_prepare(
                    &second,
                    intent.operation_id(),
                    intent.manifest_digest(),
                    MetadataCommitPhase::Finalize
                )
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::DigestMismatch
    );
    assert_eq!(
        rejected(repository.finalize(&intent).await.unwrap_err()),
        SnapshotErrorCode::DigestMismatch
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        prepared.dag().payloads().len() as u64
    );
    assert_eq!(
        mst2_retention_node::Entity::find()
            .all(&second)
            .await
            .unwrap()
            .into_iter()
            .map(|node| (node.node_id, node.incoming_refs))
            .collect::<BTreeSet<_>>(),
        counters
    );
    assert_eq!(
        mst2_metadata_prepare::Entity::find_by_id(intent.prepare_id().to_owned())
            .one(&second)
            .await
            .unwrap()
            .unwrap()
            .state,
        "COMMITTED"
    );
    overwrite_installed_payload_for_test(&first, page, page.bytes.clone()).await;
    assert_eq!(
        repository
            .inspect_prepare(
                &second,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap(),
        MetadataPrepareObservation::Committed(receipt)
    );
}

#[tokio::test]
async fn native_metadata_outer_rollback_preserves_intent_but_exposes_no_graph_or_receipt() {
    let (first, second, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository
        .begin_intent("rollback", &prepared)
        .await
        .unwrap();
    install(&repository, &intent, &prepared).await;
    let dag = repository.load_installed_dag(&intent).await.unwrap();
    let txn = repository.transaction().await.unwrap();
    repository
        .finalize_in_txn(&txn, &intent, &dag)
        .await
        .unwrap();
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_metadata_prepare::Entity::find_by_id(intent.prepare_id().to_owned())
            .one(&second)
            .await
            .unwrap()
            .unwrap()
            .state,
        "PREPARING"
    );
    txn.rollback().await.unwrap();
    assert!(matches!(
        repository
            .inspect_prepare(
                &second,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap(),
        MetadataPrepareObservation::Preparing(_)
    ));
    assert_eq!(
        mst2_retention_node::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        0
    );
    repository.finalize(&intent).await.unwrap();
}

#[tokio::test]
async fn native_metadata_final_lock_rechecks_deleting_after_payload_verification() {
    let (first, second, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository
        .begin_intent("deleting", &prepared)
        .await
        .unwrap();
    install(&repository, &intent, &prepared).await;
    let dag = repository.load_installed_dag(&intent).await.unwrap();
    let graph = PostgresRetentionRepository::new(second.clone());
    graph
        .retain_group(dag.nodes(), dag.edges(), &[])
        .await
        .unwrap();
    let root = node_id(&dag.root());
    assert_eq!(
        graph.mark_deleting("gc-root", &root).await.unwrap(),
        super::super::mst2_retention::GcClaim::Marked
    );
    let txn = repository.transaction().await.unwrap();
    assert_eq!(
        repository
            .finalize_in_txn(&txn, &intent, &dag)
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::ObjectUnavailable
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_metadata_prepare::Entity::find_by_id(intent.prepare_id().to_owned())
            .one(&second)
            .await
            .unwrap()
            .unwrap()
            .state,
        "PREPARING"
    );
    assert_eq!(graph.node(&root).await.unwrap().unwrap().state, "DELETING");
}

#[tokio::test]
async fn native_metadata_receipt_replay_rejects_lost_pin_without_recreating_it() {
    let (first, second, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository
        .begin_intent("lost-pin", &prepared)
        .await
        .unwrap();
    install(&repository, &intent, &prepared).await;
    repository.finalize(&intent).await.unwrap();
    PostgresRetentionRepository::new(first)
        .release_root(&RetentionRoot::Prepare(intent.prepare_id().into()))
        .await
        .unwrap();
    assert_eq!(
        rejected(
            repository
                .inspect_prepare(
                    &second,
                    intent.operation_id(),
                    intent.manifest_digest(),
                    MetadataCommitPhase::Finalize
                )
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(
        rejected(repository.finalize(&intent).await.unwrap_err()),
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_metadata_payload::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn native_metadata_recovery_lock_timeout_is_unknown_and_never_releases_pins() {
    let (first, second, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository.begin_intent("timeout", &prepared).await.unwrap();
    install(&repository, &intent, &prepared).await;
    repository.finalize(&intent).await.unwrap();
    let blocker = repository.transaction().await.unwrap();
    repository.barrier(&blocker).await.unwrap();
    let mut recovery = PostgresMetadataInstallRepository::new(second.clone())
        .await
        .unwrap();
    recovery.barrier_timeout = Duration::from_millis(25);
    assert!(matches!(
        recovery
            .inspect_prepare(
                &second,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap_err(),
        MetadataInstallError::CommitUncertain {
            phase: MetadataCommitPhase::Finalize,
            ..
        }
    ));
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        2
    );
    blocker.rollback().await.unwrap();
    assert!(matches!(
        recovery
            .inspect_prepare(
                &second,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap(),
        MetadataPrepareObservation::Committed(_)
    ));
}

#[tokio::test]
async fn native_metadata_stored_identity_and_plan_tampering_fail_closed_on_restart() {
    let (first, second, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let intent = repository
        .begin_intent("profile", &prepared("/"))
        .await
        .unwrap();
    first
        .execute_unprepared("UPDATE mst2_metadata_prepare SET projection_revision=2")
        .await
        .unwrap();
    assert_eq!(
        rejected(
            repository
                .inspect_prepare(
                    &second,
                    intent.operation_id(),
                    intent.manifest_digest(),
                    MetadataCommitPhase::Intent
                )
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    first.execute_unprepared("UPDATE mst2_metadata_prepare SET projection_revision=1,canonical_plan=canonical_plan || decode('ff','hex')").await.unwrap();
    assert_eq!(
        rejected(
            repository
                .inspect_prepare(
                    &second,
                    intent.operation_id(),
                    intent.manifest_digest(),
                    MetadataCommitPhase::Intent
                )
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        0
    );
}

#[derive(Clone, Copy)]
enum Fault {
    BeforeCommit,
    AfterCommit,
    PrepareRead,
}

struct PgCommitFaultProxy {
    url: String,
    armed: Arc<AtomicBool>,
    fired: Arc<AtomicBool>,
    commit_observed: Arc<AtomicBool>,
    changed: Arc<Notify>,
    task: JoinHandle<()>,
}

impl PgCommitFaultProxy {
    async fn start(original: &str, fault: Fault) -> Self {
        let mut url = url::Url::parse(original).unwrap();
        let host = url.host_str().unwrap().to_owned();
        let port = url.port().unwrap_or(5432);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        url.set_host(Some("127.0.0.1")).unwrap();
        url.set_port(Some(address.port())).unwrap();
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key != "sslmode")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("sslmode", "disable");
        let armed = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let commit_observed = Arc::new(AtomicBool::new(false));
        let changed = Arc::new(Notify::new());
        let a = armed.clone();
        let f = fired.clone();
        let c = commit_observed.clone();
        let n = changed.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (client,_)=accepted.unwrap();let host=host.clone();
                        let a=a.clone();let f=f.clone();let c=c.clone();let n=n.clone();
                        connections.spawn(async move {
                            let server=TcpStream::connect((host.as_str(),port)).await?;
                            forward_connection(client,server,fault,a,f,c,n).await
                        });
                    }
                    _=connections.join_next(),if !connections.is_empty()=>{}
                }
            }
        });
        Self {
            url: url.to_string(),
            armed,
            fired,
            commit_observed,
            changed,
            task,
        }
    }
    async fn wait_for_fault(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let changed = self.changed.notified();
                if self.fired.load(Ordering::SeqCst) {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("proxy never observed the real COMMIT fault");
    }
}
impl Drop for PgCommitFaultProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<(u8, Vec<u8>)> {
    let kind = reader.read_u8().await?;
    let length = reader.read_u32().await?;
    if !(4..=4 * 1024 * 1024).contains(&length) {
        return Err(std::io::Error::other("invalid PostgreSQL frame length"));
    }
    let mut body = vec![0; length as usize - 4];
    reader.read_exact(&mut body).await?;
    Ok((kind, body))
}
async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    body: &[u8],
) -> std::io::Result<()> {
    writer.write_u8(kind).await?;
    writer.write_u32(body.len() as u32 + 4).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

async fn forward_connection(
    mut client: TcpStream,
    mut server: TcpStream,
    fault: Fault,
    armed: Arc<AtomicBool>,
    fired: Arc<AtomicBool>,
    commit_observed: Arc<AtomicBool>,
    changed: Arc<Notify>,
) -> std::io::Result<()> {
    // sslmode=disable makes the first message a bounded StartupMessage.
    let length = client.read_u32().await?;
    if !(8..=65536).contains(&length) {
        return Err(std::io::Error::other("invalid PostgreSQL startup length"));
    }
    let mut startup = vec![0; length as usize - 4];
    client.read_exact(&mut startup).await?;
    server.write_u32(length).await?;
    server.write_all(&startup).await?;
    server.flush().await?;
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let suppress = Arc::new(AtomicBool::new(false));
    let sender_suppress = suppress.clone();
    let (stop, mut stopped) = watch::channel(false);
    let sender_stop = stop.clone();
    let sender_fired = fired.clone();
    let sender_changed = changed.clone();
    let to_server = async move {
        loop {
            let frame = tokio::select! {result=read_frame(&mut cr)=>result?, _=stopped.changed()=>return Ok::<_,std::io::Error>(())};
            let is_commit = frame.0 == b'Q' && frame.1.as_slice() == b"COMMIT\0";
            let fault_target = match fault {
                Fault::PrepareRead => {
                    b"PQ".contains(&frame.0)
                        && frame
                            .1
                            .windows(b"mst2_metadata_prepare".len())
                            .any(|bytes| bytes == b"mst2_metadata_prepare")
                }
                _ => is_commit,
            };
            if fault_target && armed.swap(false, Ordering::SeqCst) {
                match fault {
                    Fault::BeforeCommit | Fault::PrepareRead => {
                        sender_fired.store(true, Ordering::SeqCst);
                        sender_changed.notify_one();
                        sw.shutdown().await?;
                        sender_stop.send_replace(true);
                        return Ok(());
                    }
                    Fault::AfterCommit => sender_suppress.store(true, Ordering::SeqCst),
                }
            }
            write_frame(&mut sw, frame.0, &frame.1).await?;
        }
    };
    let mut stopped = stop.subscribe();
    let to_client = async move {
        loop {
            let frame = tokio::select! {result=read_frame(&mut sr)=>result?, _=stopped.changed()=>{cw.shutdown().await?;return Ok::<_,std::io::Error>(());}};
            if suppress.load(Ordering::SeqCst) {
                if frame.0 == b'C' && frame.1.as_slice() == b"COMMIT\0" {
                    commit_observed.store(true, Ordering::SeqCst);
                    fired.store(true, Ordering::SeqCst);
                    changed.notify_one();
                    cw.shutdown().await?;
                    stop.send_replace(true);
                    return Ok(());
                }
            } else {
                write_frame(&mut cw, frame.0, &frame.1).await?;
            }
        }
    };
    tokio::try_join!(to_server, to_client)?;
    Ok(())
}

async fn commit_fault_case(fault: Fault) {
    let (direct, recovery, _schema, url) = fixture().await;
    let proxy = PgCommitFaultProxy::start(&url, fault).await;
    let mut options = sea_orm::ConnectOptions::new(proxy.url.clone());
    options.max_connections(1).min_connections(1);
    let proxied = Database::connect(options).await.unwrap();
    let repository = PostgresMetadataInstallRepository::new(proxied)
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository
        .begin_intent("commit-fault", &prepared)
        .await
        .unwrap();
    install(&repository, &intent, &prepared).await;
    proxy.armed.store(true, Ordering::SeqCst);
    let error = tokio::time::timeout(Duration::from_secs(15), repository.finalize(&intent))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        error,
        MetadataInstallError::CommitUncertain {
            phase: MetadataCommitPhase::Finalize,
            ..
        }
    ));
    proxy.wait_for_fault().await;
    let observed = repository
        .inspect_prepare(
            &recovery,
            intent.operation_id(),
            intent.manifest_digest(),
            MetadataCommitPhase::Finalize,
        )
        .await
        .unwrap();
    match fault {
        Fault::AfterCommit => {
            assert!(proxy.commit_observed.load(Ordering::SeqCst));
            assert!(matches!(observed, MetadataPrepareObservation::Committed(_)));
            assert_eq!(
                mst2_retention_root::Entity::find()
                    .count(&direct)
                    .await
                    .unwrap(),
                2
            );
            assert_eq!(
                mst2_retention_edge::Entity::find()
                    .count(&direct)
                    .await
                    .unwrap(),
                1
            );
            let restarted = PostgresMetadataInstallRepository::new(direct.clone())
                .await
                .unwrap();
            restarted.finalize(&intent).await.unwrap();
            assert_eq!(
                mst2_retention_edge::Entity::find()
                    .count(&direct)
                    .await
                    .unwrap(),
                1
            );
        }
        Fault::BeforeCommit => {
            assert!(!proxy.commit_observed.load(Ordering::SeqCst));
            assert!(matches!(observed, MetadataPrepareObservation::Preparing(_)));
            assert_eq!(
                mst2_retention_root::Entity::find()
                    .count(&direct)
                    .await
                    .unwrap(),
                0
            );
            assert_eq!(
                mst2_retention_node::Entity::find()
                    .count(&direct)
                    .await
                    .unwrap(),
                0
            );
            PostgresMetadataInstallRepository::new(direct)
                .await
                .unwrap()
                .finalize(&intent)
                .await
                .unwrap();
        }
        Fault::PrepareRead => panic!("query fault uses its independent recovery test"),
    }
}

#[tokio::test]
async fn native_metadata_real_commit_response_loss_recovers_receipt_without_releasing_pin() {
    commit_fault_case(Fault::AfterCommit).await;
}

#[tokio::test]
async fn native_metadata_real_connection_loss_before_commit_recovers_preparing_without_false_success()
 {
    commit_fault_case(Fault::BeforeCommit).await;
}

#[tokio::test]
async fn native_metadata_wrong_schema_primary_cannot_report_absent_for_another_committed_operation()
{
    let (first, second, _schema, _url) = fixture().await;
    let (wrong_primary, _other, _wrong_schema, _wrong_url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository.begin_intent("scope", &prepared).await.unwrap();
    install(&repository, &intent, &prepared).await;
    repository.finalize(&intent).await.unwrap();
    assert!(matches!(
        repository
            .inspect_prepare(
                &wrong_primary,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap_err(),
        MetadataInstallError::CommitUncertain { .. }
    ));
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        mst2_metadata_prepare::Entity::find()
            .count(&wrong_primary)
            .await
            .unwrap(),
        0
    );
    assert!(matches!(
        repository
            .inspect_prepare(
                &second,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap(),
        MetadataPrepareObservation::Committed(_)
    ));
}

#[tokio::test]
async fn native_metadata_query_loss_after_recovery_barrier_preserves_typed_unknown() {
    let (first, second, _schema, url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let prepared = prepared("/");
    let intent = repository
        .begin_intent("read-fault", &prepared)
        .await
        .unwrap();
    install(&repository, &intent, &prepared).await;
    repository.finalize(&intent).await.unwrap();
    let proxy = PgCommitFaultProxy::start(&url, Fault::PrepareRead).await;
    let mut options = sea_orm::ConnectOptions::new(proxy.url.clone());
    options.max_connections(1).min_connections(1);
    let fresh = Database::connect(options).await.unwrap();
    proxy.armed.store(true, Ordering::SeqCst);
    assert!(matches!(
        repository
            .inspect_prepare(
                &fresh,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap_err(),
        MetadataInstallError::CommitUncertain { .. }
    ));
    proxy.wait_for_fault().await;
    assert!(!proxy.commit_observed.load(Ordering::SeqCst));
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&second)
            .await
            .unwrap(),
        2
    );
    assert!(matches!(
        repository
            .inspect_prepare(
                &second,
                intent.operation_id(),
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap(),
        MetadataPrepareObservation::Committed(_)
    ));
}
