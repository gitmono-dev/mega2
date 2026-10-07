use std::io;

use tokio::{sync::Notify, time::timeout};

use super::*;
use crate::orbit_api::error::IoOrbitError;

#[derive(Clone)]
pub(super) struct ChunkFault {
    pub(super) oid: String,
    pub(super) size: u64,
    pattern: Bytes,
    mode: RangeMode,
    requests: Arc<std::sync::Mutex<Vec<(String, u64, u64)>>>,
}

#[derive(Clone)]
enum RangeMode {
    Good,
    WrongMeta,
    WrongDigest,
    Truncated,
    TooLong,
    LateError,
    Unsupported,
    WrongOffset,
    Missing,
    Held {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        drops: Arc<AtomicUsize>,
    },
}

struct DropCount(Arc<AtomicUsize>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl ChunkFault {
    pub(super) fn full_stream(self) -> ObjectByteStream {
        Box::pin(futures::stream::unfold(
            (self.pattern, 0u64, self.size),
            |(pattern, offset, size)| async move {
                if offset == size {
                    return None;
                }
                let len = (size - offset).min(CHUNK_SIZE as u64) as usize;
                let bytes = pattern.slice(..len);
                Some((Ok(bytes), (pattern, offset + len as u64, size)))
            },
        ))
    }

    pub(super) fn range_stream(
        self,
        start: u64,
        end: u64,
    ) -> OrbitResult<Option<(ObjectByteStream, ObjectMeta)>> {
        self.requests
            .lock()
            .unwrap()
            .push((self.oid.clone(), start, end));
        if matches!(self.mode, RangeMode::Unsupported) {
            return Ok(None);
        }
        if matches!(self.mode, RangeMode::WrongOffset) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong backend range").into());
        }
        if matches!(self.mode, RangeMode::Missing) {
            return Err(IoOrbitError::object_store_not_found("fixed raw missing"));
        }
        assert!(start < end && end <= self.size && end - start <= CHUNK_SIZE as u64);
        assert_eq!(start % CHUNK_SIZE as u64, 0);
        let len = (end - start) as usize;
        let raw = self.pattern.slice(..len);
        let meta = ObjectMeta {
            size: if matches!(self.mode, RangeMode::WrongMeta) {
                self.size as i64 + 1
            } else {
                self.size as i64
            },
            ..Default::default()
        };
        let stream: ObjectByteStream = match self.mode {
            RangeMode::WrongDigest => {
                Box::pin(futures::stream::iter([Ok(Bytes::from(vec![0; len]))]))
            }
            RangeMode::Truncated => Box::pin(futures::stream::iter([Ok(raw.slice(..len - 1))])),
            RangeMode::TooLong => Box::pin(futures::stream::iter([
                Ok(raw),
                Ok(Bytes::from_static(b"!")),
            ])),
            RangeMode::LateError => Box::pin(futures::stream::iter([
                Ok(raw),
                Err(io::Error::other("late range error")),
            ])),
            RangeMode::Held {
                entered,
                release,
                drops,
            } => Box::pin(futures::stream::unfold(
                (Some(raw), entered, release, DropCount(drops)),
                |(raw, entered, release, owner)| async move {
                    let raw = raw?;
                    entered.notify_one();
                    release.notified().await;
                    Some((Ok(raw), (None, entered, release, owner)))
                },
            )),
            _ => Box::pin(futures::stream::iter([Ok(raw)])),
        };
        Ok(Some((stream, meta)))
    }
}

async fn oid_for(fixture: &Fixture, path: &str) -> String {
    let handler = MonoApiService::from(&fixture.state);
    let main = fixture
        .state
        .storage
        .mono_storage()
        .get_main_ref("/project")
        .await
        .unwrap()
        .unwrap();
    let tree = handler.get_tree_by_hash(&main.ref_tree_hash).await.unwrap();
    match resolve_abs_metadata(&handler, &tree, path).await.unwrap() {
        MetadataWalkOutcome::FoundFile { oid, .. } => oid,
        other => panic!("fixture did not resolve: {other:?}"),
    }
}

async fn set_fact(fixture: &Fixture, oid: &str, size: u64, digest: [u8; 32]) {
    let db = fixture.state.storage.mono_storage().get_connection();
    let fact = mst2_verified_object::Entity::find()
        .filter(mst2_verified_object::Column::GitOid.eq(oid))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut fact = fact.into_active_model();
    fact.size = Set(size as i64);
    fact.raw_sha256 = Set(digest.to_vec());
    fact.update(db).await.unwrap();
}

fn request(path: &str, digest: [u8; 32], map_id: &str, index: u64) -> Value {
    json!({"items":[{"path":path,"expected_digest":format!("sha256:{}",hex_of(&digest)),"map_id":map_id,"chunk_index":index.to_string()}],"encoding":"identity"})
}

async fn assert_chunk(response: Response, body: &Value, raw: &[u8], index: u64) {
    assert_eq!(response.status(), 200);
    let wire = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let frames = parse_stream(&wire).unwrap();
    let [Frame::Chunk(chunk), Frame::End(end)] = frames.as_slice() else {
        panic!("one CHUNK and END required");
    };
    assert_eq!(chunk.chunk_bytes, raw);
    assert_eq!(chunk.chunk_index, index);
    assert_eq!(end.request_item_count, 1);
    assert_eq!(end.unique_unit_count, 1);
    assert_eq!(end.logical_bytes, raw.len() as u64);
    assert_eq!(
        end.request_body_sha256,
        <[u8; 32]>::from(Sha256::digest(body.to_string().as_bytes()))
    );
}

#[tokio::test]
async fn mst2_large_chunk_uses_current_oid_strict_range_faults_cancel_retry_and_lease() {
    let fixture = Fixture::new_with_pg_config_directories_and_objects(
        false,
        0,
        &[("other".to_string(), vec![19; 4096])],
    )
    .await;
    let mut pattern = vec![51; CHUNK_SIZE as usize];
    let seed = uuid::Uuid::new_v4();
    pattern[..16].copy_from_slice(seed.as_bytes());
    let pattern = Bytes::from(pattern);
    let size = 512 * CHUNK_SIZE as u64 + 7;
    let mut hash = Sha256::new();
    for _ in 0..512 {
        hash.update(&pattern);
    }
    hash.update(&pattern[..7]);
    let digest: [u8; 32] = hash.finalize().into();
    let other = oid_for(&fixture, "/other").await;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    for oid in [&fixture.oid, &other] {
        set_fact(&fixture, oid, size, digest).await;
        fixture
            .counts
            .chunk_faults
            .lock()
            .unwrap()
            .push(ChunkFault {
                oid: oid.clone(),
                size,
                pattern: pattern.clone(),
                mode: RangeMode::Good,
                requests: requests.clone(),
            });
    }
    fixture.counts.reset();
    let map = fixture.map("/file").await;
    assert_eq!(map["map"]["file_size"], size.to_string());
    fixture.counts.assert(1, size as usize);
    let map_id = map["map"]["map_id"].as_str().unwrap();
    fixture.counts.reset();
    // Equal content map sharing never carries the first source's OID.
    let body = request("/other", digest, map_id, 512);
    assert_chunk(
        fixture
            .send("POST", "chunks", Body::from(body.to_string()))
            .await,
        &body,
        &pattern[..7],
        512,
    )
    .await;
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        &[(other.clone(), 512 * CHUNK_SIZE as u64, size)]
    );
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.bytes.load(Ordering::SeqCst), 7);
    for (index, encoding) in [(0, "identity"), (511, "zstd")] {
        let mut full = request("/other", digest, map_id, index);
        full["encoding"] = json!(encoding);
        fixture.counts.reset();
        assert_chunk(
            fixture
                .send("POST", "chunks", Body::from(full.to_string()))
                .await,
            &full,
            &pattern,
            index,
        )
        .await;
        assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.counts.bytes.load(Ordering::SeqCst),
            CHUNK_SIZE as usize
        );
        assert_eq!(
            requests.lock().unwrap().last().unwrap(),
            &(
                other.clone(),
                index * CHUNK_SIZE as u64,
                (index + 1) * CHUNK_SIZE as u64
            )
        );
    }
    for (mode, status, code, bytes) in [
        (RangeMode::WrongMeta, 502, "INTEGRITY_ERROR", 0),
        (RangeMode::WrongDigest, 502, "INTEGRITY_ERROR", 7),
        (RangeMode::Truncated, 502, "INTEGRITY_ERROR", 6),
        (RangeMode::TooLong, 502, "INTEGRITY_ERROR", 8),
        (RangeMode::LateError, 503, "OBJECT_UNAVAILABLE", 7),
        (RangeMode::Unsupported, 400, "RANGE_NOT_SUPPORTED", 0),
        (RangeMode::WrongOffset, 502, "INTEGRITY_ERROR", 0),
        (RangeMode::Missing, 503, "OBJECT_UNAVAILABLE", 0),
    ] {
        fixture.counts.chunk_faults.lock().unwrap()[1].mode = mode;
        fixture.counts.reset();
        error(
            fixture
                .send("POST", "chunks", Body::from(body.to_string()))
                .await,
            status,
            code,
            false,
        )
        .await;
        assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.counts.bytes.load(Ordering::SeqCst), bytes);
    }
    fixture.counts.chunk_faults.lock().unwrap()[1].mode = RangeMode::LateError;
    let first = request("/file", digest, map_id, 0);
    let batch = json!({"items":[first["items"][0],body["items"][0]],"encoding":"identity"});
    fixture.counts.reset();
    error(
        fixture
            .send("POST", "chunks", Body::from(batch.to_string()))
            .await,
        503,
        "OBJECT_UNAVAILABLE",
        false,
    )
    .await;
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 2);
    assert_eq!(
        fixture.counts.bytes.load(Ordering::SeqCst),
        CHUNK_SIZE as usize + 7
    );
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let drops = Arc::new(AtomicUsize::new(0));
    fixture.counts.chunk_faults.lock().unwrap()[1].mode = RangeMode::Held {
        entered: entered.clone(),
        release: release.clone(),
        drops: drops.clone(),
    };
    let task = tokio::spawn(fixture.app.clone().oneshot(fixture.request(
        "POST",
        "chunks",
        Body::from(body.to_string()),
    )));
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.err().unwrap().is_cancelled());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    fixture.counts.chunk_faults.lock().unwrap()[1].mode = RangeMode::Good;
    assert_chunk(
        fixture
            .send("POST", "chunks", Body::from(body.to_string()))
            .await,
        &body,
        &pattern[..7],
        512,
    )
    .await;
    // Revocation while the range is held is a pre-header 410 JSON response.
    fixture.counts.chunk_faults.lock().unwrap()[1].mode = RangeMode::Held {
        entered: entered.clone(),
        release: release.clone(),
        drops: drops.clone(),
    };
    let task = tokio::spawn(fixture.app.clone().oneshot(fixture.request(
        "POST",
        "chunks",
        Body::from(body.to_string()),
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
    error(
        timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        410,
        "LEASE_EXPIRED",
        false,
    )
    .await;
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn mst2_chunk_batch_live_budget_and_invalid_later_path_reject_before_body_io() {
    let fixture = Fixture::new_with_pg_config_directories_and_objects(
        false,
        0,
        &[
            ("one".to_string(), vec![11; 4096]),
            ("two".to_string(), vec![12; 4096]),
        ],
    )
    .await;
    let mut items = Vec::new();
    for (index, path) in ["/file", "/one", "/two"].iter().enumerate() {
        let oid = oid_for(&fixture, path).await;
        let digest = [index as u8 + 1; 32];
        set_fact(&fixture, &oid, 512 * CHUNK_SIZE as u64, digest).await;
        items.push(
            request(
                path,
                digest,
                &format!("sha256:{}", hex_of(&[index as u8 + 1; 32])),
                0,
            )["items"][0]
                .clone(),
        );
    }
    fixture.counts.reset();
    error(
        fixture
            .send(
                "POST",
                "chunks",
                Body::from(json!({"items":items,"encoding":"identity"}).to_string()),
            )
            .await,
        413,
        "LIMIT_EXCEEDED",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    let fixture = Fixture::new().await;
    let mut body = fixture.chunk_body("/file", &format!("sha256:{}", hex_of(&[1; 32])), "0");
    let mut invalid = body["items"][0].clone();
    invalid["path"] = json!("/absent");
    body["items"].as_array_mut().unwrap().push(invalid);
    error(
        fixture
            .send("POST", "chunks", Body::from(body.to_string()))
            .await,
        404,
        "PATH_NOT_FOUND",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_budgeted_chunk_body_preserves_per_frame_revocation_and_no_end() {
    let fixture = Fixture::new().await;
    let map = fixture.map("/file").await;
    let map_id = map["map"]["map_id"].as_str().unwrap();
    let mut body = fixture.chunk_body("/file", map_id, "0");
    body["items"]
        .as_array_mut()
        .unwrap()
        .push(fixture.chunk_body("/file", map_id, "1")["items"][0].clone());
    let response = fixture
        .send("POST", "chunks", Body::from(body.to_string()))
        .await;
    assert_eq!(response.status(), 200);
    let mut data = response.into_body().into_data_stream();
    let first = data.next().await.unwrap().unwrap();
    assert!(matches!(
        parse_stream(&first).unwrap().as_slice(),
        [Frame::Chunk(_)]
    ));
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
    assert!(data.next().await.unwrap().is_err());
    assert!(data.next().await.is_none());
}
