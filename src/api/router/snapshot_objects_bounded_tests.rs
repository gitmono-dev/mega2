use std::io;

use tokio::{sync::Notify, time::timeout};

use super::*;

#[derive(Clone)]
pub(super) struct StreamFault {
    pub(super) oid: String,
    pub(super) kind: FaultKind,
}

#[derive(Clone)]
pub(super) enum FaultKind {
    Parts(Vec<Bytes>),
    LateError(Bytes),
    Oversized(Bytes, Arc<AtomicUsize>),
    Held {
        raw: Bytes,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        drops: Arc<AtomicUsize>,
    },
    HeldThenError {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        drops: Arc<AtomicUsize>,
    },
    HeldFragment {
        prefix: Bytes,
        fragment: Option<Bytes>,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        tail_polls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    },
}

struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl StreamFault {
    pub(super) fn stream(self) -> ObjectByteStream {
        match self.kind {
            FaultKind::HeldFragment {
                prefix,
                fragment,
                entered,
                release,
                tail_polls,
                drops,
            } => Box::pin(futures::stream::unfold(
                (
                    prefix,
                    fragment,
                    entered,
                    release,
                    tail_polls,
                    DropCount(drops),
                    0u8,
                ),
                |(prefix, fragment, entered, release, tail_polls, owner, turn)| async move {
                    match turn {
                        0 => Some((
                            Ok(prefix.clone()),
                            (prefix, fragment, entered, release, tail_polls, owner, 1),
                        )),
                        1 => {
                            entered.notify_one();
                            release.notified().await;
                            fragment.clone().map(|part| {
                                (
                                    Ok(part),
                                    (prefix, fragment, entered, release, tail_polls, owner, 2),
                                )
                            })
                        }
                        _ => {
                            tail_polls.fetch_add(1, Ordering::SeqCst);
                            std::future::pending().await
                        }
                    }
                },
            )),
            FaultKind::HeldThenError {
                entered,
                release,
                drops,
            } => Box::pin(futures::stream::unfold(
                (true, entered, release, DropCount(drops)),
                |(first, entered, release, owner)| async move {
                    if !first {
                        return None;
                    }
                    entered.notify_one();
                    release.notified().await;
                    Some((
                        Err(io::Error::other("held source read failed")),
                        (false, entered, release, owner),
                    ))
                },
            )),
            FaultKind::Parts(parts) => Box::pin(futures::stream::iter(parts.into_iter().map(Ok))),
            FaultKind::LateError(raw) => Box::pin(futures::stream::iter([
                Ok(raw),
                Err(io::Error::other("test late object stream failure")),
            ])),
            FaultKind::Oversized(raw, tail_polls) => Box::pin(futures::stream::unfold(
                (Some(raw), tail_polls),
                |(raw, tail_polls)| async move {
                    match raw {
                        Some(raw) => Some((Ok(raw), (None, tail_polls))),
                        None => {
                            tail_polls.fetch_add(1, Ordering::SeqCst);
                            Some((
                                Err(io::Error::other("oversized tail must not be polled")),
                                (None, tail_polls),
                            ))
                        }
                    }
                },
            )),
            FaultKind::Held {
                raw,
                entered,
                release,
                drops,
            } => Box::pin(futures::stream::unfold(
                (raw, entered, release, DropCount(drops), 0u8),
                |(raw, entered, release, drop_count, turn)| async move {
                    match turn {
                        0 => Some((Ok(raw.slice(..1)), (raw, entered, release, drop_count, 1))),
                        1 => {
                            entered.notify_one();
                            release.notified().await;
                            Some((Ok(raw.slice(1..)), (raw, entered, release, drop_count, 2)))
                        }
                        _ => None,
                    }
                },
            )),
        }
    }
}

fn files(count: usize, size: usize) -> Vec<(String, Vec<u8>)> {
    let seed = uuid::Uuid::new_v4();
    (0..count)
        .map(|index| {
            let mut raw = vec![index as u8; size];
            let prefix = format!("blob 3\0abc\0{seed}-{index}");
            raw[..prefix.len()].copy_from_slice(prefix.as_bytes());
            (format!("object-{index:03}"), raw)
        })
        .collect()
}

async fn fixture(objects: &[(String, Vec<u8>)]) -> Fixture {
    Fixture::new_with_pg_config_directories_and_objects(false, 0, objects).await
}

fn body(objects: &[(String, Vec<u8>)]) -> Body {
    Body::from(request_bytes(objects))
}

fn request_bytes(objects: &[(String, Vec<u8>)]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "items":objects.iter().map(|(name, raw)| json!({
            "path":format!("/{name}"),
            "expected_digest":format!("sha256:{}", hex_of(&digest(raw))),
        })).collect::<Vec<_>>(),
        "encoding":"identity",
    }))
    .unwrap()
}

async fn object_oid(fixture: &Fixture, path: &str) -> String {
    let handler = MonoApiService::from(&fixture.state);
    let context = fixture
        .state
        .storage
        .mono_storage()
        .get_main_ref("/project")
        .await
        .unwrap()
        .unwrap();
    let tree = handler
        .get_tree_by_hash(&context.ref_tree_hash)
        .await
        .unwrap();
    match resolve_abs_metadata(&handler, &tree, path).await.unwrap() {
        MetadataWalkOutcome::FoundFile { oid, .. } => oid,
        other => panic!("object test path did not resolve: {other:?}"),
    }
}

async fn install_fault(fixture: &Fixture, path: &str, kind: FaultKind) {
    let oid = object_oid(fixture, path).await;
    *fixture.counts.object_fault.lock().unwrap() = Some(StreamFault { oid, kind });
}

async fn assert_objects(response: Response, request: &[u8], objects: &[(String, Vec<u8>)]) {
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-mega-request-digest"],
        format!("sha256:{}", hex_of(&digest(request)))
    );
    let encoded = to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let frames = parse_stream(&encoded).unwrap();
    let mut actual = Vec::new();
    let mut end = None;
    for frame in frames {
        match frame {
            Frame::Object(payload) => {
                assert!(end.is_none(), "END must be terminal");
                actual.extend(payload.objects);
            }
            Frame::End(record) => {
                assert!(end.is_none());
                end = Some(record);
            }
            other => panic!("unexpected OBJECT frame: {other:?}"),
        }
    }
    let mut expected = Vec::new();
    for (_, raw) in objects {
        let id = digest(raw);
        if !expected.iter().any(|(digest, _)| *digest == id) {
            expected.push((id, raw.clone()));
        }
    }
    assert_eq!(actual, expected);
    let end = end.unwrap();
    assert_eq!(end.request_item_count, objects.len() as u32);
    assert_eq!(end.unique_unit_count, expected.len() as u32);
    assert_eq!(
        end.logical_bytes,
        expected
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>()
    );
    assert_eq!(end.request_body_sha256, digest(request));
}

#[tokio::test]
async fn oversized_item_and_later_invalid_path_reject_entire_batch_before_body_io() {
    let fixture = Fixture::new().await;
    let link_digest = format!("sha256:{}", hex_of(&digest(b"file")));
    for last in [
        json!({"path":"/file","expected_digest":fixture.digest_string()}),
        json!({"path":"/missing","expected_digest":link_digest}),
    ] {
        let oversized = last["path"] == "/file";
        let request = json!({"items":[
            {"path":"/link","expected_digest":link_digest},last,
        ]});
        error(
            fixture
                .send("POST", "objects", Body::from(request.to_string()))
                .await,
            if oversized { 400 } else { 404 },
            if oversized {
                "SCOPE_INVALID"
            } else {
                "PATH_NOT_FOUND"
            },
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
}

#[tokio::test]
async fn unique_batch_over_eight_mib_rejects_before_any_body_io() {
    let objects = files(33, 256 * 1024);
    let fixture = fixture(&objects).await;
    error(
        fixture.send("POST", "objects", body(&objects)).await,
        400,
        "SCOPE_INVALID",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn cap_boundaries_and_empty_object_keep_exact_raw_bytes_and_end_counts() {
    let mut objects = files(32, 256 * 1024);
    objects.push(("empty".into(), Vec::new()));
    let fixture = fixture(&objects[..32]).await;
    let request = request_bytes(&objects);
    assert_objects(
        fixture
            .send("POST", "objects", Body::from(request.clone()))
            .await,
        &request,
        &objects,
    )
    .await;
    fixture.counts.assert(33, 8 * 1024 * 1024);
}

#[tokio::test]
async fn every_alias_is_admitted_and_exact_oid_body_is_loaded_once() {
    let raw = files(1, 8192).remove(0).1;
    let objects: Vec<_> = (0..128)
        .map(|i| (format!("alias-{i:03}"), raw.clone()))
        .collect();
    let fixture = fixture(&objects).await;
    let request = request_bytes(&objects);
    assert_objects(
        fixture
            .send("POST", "objects", Body::from(request.clone()))
            .await,
        &request,
        &objects,
    )
    .await;
    fixture.counts.assert(1, raw.len());
    fixture.counts.reset();
    let mut request: Value = serde_json::from_slice(&request).unwrap();
    request["items"][127]["expected_digest"] = json!(format!("sha256:{}", "00".repeat(32)));
    error(
        fixture
            .send("POST", "objects", Body::from(request.to_string()))
            .await,
        409,
        "EXPECTED_DIGEST_MISMATCH",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn conflicting_sizes_reject_before_io_and_distinct_oids_still_verify_each_body() {
    let objects = files(2, 8192);
    let fixture = fixture(&objects).await;
    let oid = object_oid(&fixture, "/object-001").await;
    let db = fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .clone();
    let fact = mst2_verified_object::Entity::find()
        .filter(mst2_verified_object::Column::GitOid.eq(oid))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut fact = fact.into_active_model();
    fact.raw_sha256 = Set(digest(&objects[0].1).to_vec());
    fact.size = Set(8193);
    let fact = fact.update(&db).await.unwrap();
    let request = json!({"items":[
        {"path":"/object-000","expected_digest":format!("sha256:{}", hex_of(&digest(&objects[0].1)))},
        {"path":"/object-001","expected_digest":format!("sha256:{}", hex_of(&digest(&objects[0].1)))},
    ]});
    error(
        fixture
            .send("POST", "objects", Body::from(request.to_string()))
            .await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    let mut fact = fact.into_active_model();
    fact.size = Set(8192);
    fact.update(&db).await.unwrap();
    error(
        fixture
            .send("POST", "objects", Body::from(request.to_string()))
            .await,
        409,
        "EXPECTED_DIGEST_MISMATCH",
        false,
    )
    .await;
    fixture.counts.assert(2, 16384);
}

#[tokio::test]
async fn missing_or_invalid_current_facts_fail_before_body_io() {
    let fixture = Fixture::new().await;
    let request = json!({"items":[{"path":"/file","expected_digest":fixture.digest_string()}]});
    let fact = fixture.fact().await;
    fixture.delete_fact().await;
    error(
        fixture
            .send("POST", "objects", Body::from(request.to_string()))
            .await,
        503,
        "METADATA_NOT_READY",
        true,
    )
    .await;
    fixture.counts.assert(0, 0);
    let mut invalid = fact;
    invalid.raw_sha256 = vec![0; 31];
    fixture.replace_fact(invalid).await;
    error(
        fixture
            .send("POST", "objects", Body::from(request.to_string()))
            .await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn oversized_stream_chunk_is_rejected_without_copying_or_polling_tail() {
    let objects = files(1, 8192);
    let fixture = fixture(&objects).await;
    let tail_polls = Arc::new(AtomicUsize::new(0));
    install_fault(
        &fixture,
        "/object-000",
        FaultKind::Oversized(Bytes::from(vec![7; 8193]), tail_polls.clone()),
    )
    .await;
    error(
        fixture.send("POST", "objects", body(&objects)).await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(1, 8193);
    assert_eq!(tail_polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn truncated_wrong_sha_and_late_stream_error_never_produce_200() {
    let objects = files(1, 8192);
    let fixture = fixture(&objects).await;
    for (fault, status, code, bytes) in [
        (
            FaultKind::Parts(vec![Bytes::copy_from_slice(&objects[0].1[..8191])]),
            502,
            "INTEGRITY_ERROR",
            8191,
        ),
        (
            FaultKind::Parts(vec![Bytes::from(vec![0; 8192])]),
            409,
            "EXPECTED_DIGEST_MISMATCH",
            8192,
        ),
        (
            FaultKind::LateError(Bytes::copy_from_slice(&objects[0].1)),
            500,
            "INTERNAL",
            8192,
        ),
        (
            FaultKind::Parts(vec![
                Bytes::copy_from_slice(&objects[0].1),
                Bytes::from_static(b"x"),
            ]),
            502,
            "INTEGRITY_ERROR",
            8193,
        ),
    ] {
        fixture.counts.reset();
        install_fault(&fixture, "/object-000", fault).await;
        error(
            fixture.send("POST", "objects", body(&objects)).await,
            status,
            code,
            code == "INTERNAL",
        )
        .await;
        fixture.counts.assert(1, bytes);
    }
}

#[tokio::test]
async fn a_later_object_stream_failure_keeps_earlier_verified_data_unpublished() {
    let objects = files(2, 8192);
    let fixture = fixture(&objects).await;
    install_fault(
        &fixture,
        "/object-001",
        FaultKind::LateError(Bytes::copy_from_slice(&objects[1].1)),
    )
    .await;
    error(
        fixture.send("POST", "objects", body(&objects)).await,
        500,
        "INTERNAL",
        true,
    )
    .await;
    fixture.counts.assert(2, 16384);
}

#[tokio::test]
async fn missing_actual_object_body_keeps_source_error_classification_and_can_retry() {
    let objects = files(1, 8192);
    let fixture = fixture(&objects).await;
    let oid = object_oid(&fixture, "/object-000").await;
    fixture
        .state
        .storage
        .git_service
        .obj_storage
        .inner
        .delete(&ObjectKey {
            namespace: ObjectNamespace::Git,
            key: oid.clone(),
        })
        .await
        .unwrap();
    error(
        fixture.send("POST", "objects", body(&objects)).await,
        503,
        "OBJECT_UNAVAILABLE",
        false,
    )
    .await;
    fixture.counts.assert(1, 0);
    fixture
        .state
        .storage
        .git_service
        .save_object_from_model(objects[0].1.clone(), &oid)
        .await
        .unwrap();
    fixture.counts.reset();
    let request = request_bytes(&objects);
    assert_objects(
        fixture
            .send("POST", "objects", Body::from(request.clone()))
            .await,
        &request,
        &objects,
    )
    .await;
    fixture.counts.assert(1, 8192);
}

#[tokio::test]
async fn old_snapshot_objects_keep_fixed_oid_after_real_publication_advances() {
    let objects = files(1, 8192);
    let fixture = fixture(&objects).await;
    let mono = fixture.state.storage.mono_storage();
    let old = mono.get_main_ref("/project").await.unwrap().unwrap();
    let old_commit = ObjectHash::from_hex_for_kind(HashKind::Sha1, &old.ref_commit_hash).unwrap();
    let oid = object_oid(&fixture, "/object-000").await;
    let next_tree = tree(vec![item(
        TreeItemMode::Blob,
        ObjectHash::from_hex_for_kind(HashKind::Sha1, &oid).unwrap(),
        "new-only",
    )]);
    let next_commit = Commit::from_tree_id_with_kind(
        HashKind::Sha1,
        next_tree.id,
        vec![old_commit],
        "OBJECT old snapshot must retain its fixed path",
    )
    .unwrap();
    mono.save_mega_trees(vec![next_tree], next_commit.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![next_commit.clone()], None)
        .await
        .unwrap();
    publish_native_push(&fixture.state.storage, "/project", old_commit, &next_commit).await;
    let head = mono
        .read_native_publication_head(
            fixture
                .state
                .storage
                .config()
                .mst2
                .instance_uuid
                .as_deref()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.token.sequence, 2);
    assert!(head.token.certificate.is_some());
    assert_ne!(
        mono.get_main_ref("/project")
            .await
            .unwrap()
            .unwrap()
            .ref_tree_hash,
        old.ref_tree_hash
    );
    fixture.counts.reset();
    let request = request_bytes(&objects);
    assert_objects(
        fixture
            .send("POST", "objects", Body::from(request.clone()))
            .await,
        &request,
        &objects,
    )
    .await;
    fixture.counts.assert(1, 8192);
}

#[tokio::test]
async fn cancelling_actual_object_request_drops_held_stream_and_retry_succeeds() {
    let objects = files(1, 8192);
    let fixture = fixture(&objects).await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let drops = Arc::new(AtomicUsize::new(0));
    install_fault(
        &fixture,
        "/object-000",
        FaultKind::Held {
            raw: Bytes::copy_from_slice(&objects[0].1),
            entered: entered.clone(),
            release,
            drops: drops.clone(),
        },
    )
    .await;
    let task = tokio::spawn(fixture.app.clone().oneshot(fixture.request(
        "POST",
        "objects",
        body(&objects),
    )));
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    assert!(!task.is_finished());
    fixture.counts.assert(1, 1);
    task.abort();
    assert!(task.await.err().unwrap().is_cancelled());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    *fixture.counts.object_fault.lock().unwrap() = None;
    fixture.counts.reset();
    let request = request_bytes(&objects);
    assert_objects(
        fixture
            .send("POST", "objects", Body::from(request.clone()))
            .await,
        &request,
        &objects,
    )
    .await;
    fixture.counts.assert(1, 8192);
}

#[tokio::test]
async fn lease_revoked_during_object_load_cannot_deliver_verified_frames() {
    let objects = files(1, 8192);
    let fixture = fixture(&objects).await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let drops = Arc::new(AtomicUsize::new(0));
    install_fault(
        &fixture,
        "/object-000",
        FaultKind::Held {
            raw: Bytes::copy_from_slice(&objects[0].1),
            entered: entered.clone(),
            release: release.clone(),
            drops: drops.clone(),
        },
    )
    .await;
    let task = tokio::spawn(fixture.app.clone().oneshot(fixture.request(
        "POST",
        "objects",
        body(&objects),
    )));
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    let revoked = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v2/snapshots/leases/{}", fixture.lease))
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), 200);
    release.notify_one();
    let response = timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    error(response, 410, "LEASE_EXPIRED", false).await;
    fixture.counts.assert(1, 8192);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
