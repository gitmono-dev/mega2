use super::*;

use bytes::Bytes;

use crate::callisto::{
    mst2_native_head, mst2_native_publication, mst2_publication, mst2_publication_outbox,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter, Statement};

const NATIVE_INSTANCE: &str = "6ab219b0-4275-45ba-9d7b-7b0b633018cd";

async fn native_fixture() -> (tempfile::TempDir, crate::jupiter::storage::Storage, git_internal::internal::object::commit::Commit, String) {
    let temp = tempfile::tempdir().unwrap();
    let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.mst2.enabled = true;
    config.mst2.publication_enabled = true;
    config.mst2.instance_uuid = Some(NATIVE_INSTANCE.to_owned());
    let storage = crate::jupiter::tests::test_storage_with_config(temp.path(), config).await;
    let storage = crate::jupiter::tests::with_test_vault(storage, temp.path()).await;
    let name = format!("native-{}", uuid::Uuid::new_v4().simple());
    let (tip, path) = wh03_path_fixture(&storage, &name).await;
    // Persist the literal file bytes expected by the tree fixture for actual HTTP projection.
    for (oid, raw) in [("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", b"keep".as_slice()),
        ("dddddddddddddddddddddddddddddddddddddddd", b"old".as_slice())] {
        storage.git_service.save_object_from_raw(Bytes::copy_from_slice(raw)).await.unwrap();
        storage.mono_storage().get_connection().execute_raw(Statement::from_sql_and_values(
            storage.mono_storage().get_connection().get_database_backend(),
            "INSERT INTO mst2_verified_object(storage_domain,git_oid,object_kind,raw_sha256,size,verification_version,state,created_at) \
             VALUES ('git',$1,'blob',$2,$3,2,'VERIFIED',now()) ON CONFLICT DO NOTHING",
            [oid.into(), Sha256::digest(raw).to_vec().into(), (raw.len() as i64).into()],
        )).await.unwrap();
    }
    storage.mono_storage().initialize_native_publication(NATIVE_INSTANCE).await.unwrap();
    (temp, storage, tip, path)
}

async fn save_same_tree_commit(storage: &crate::jupiter::storage::Storage, parent: &git_internal::internal::object::commit::Commit) -> (String, PushPayload) {
    let commit = git_internal::internal::object::commit::Commit::from_tree_id(parent.tree_id, vec![parent.id], "same native tree n1");
    storage.mono_storage().save_mega_commits(vec![commit.clone()], None).await.unwrap();
    let oid = commit.id.to_string();
    (oid.clone(), PushPayload { commits: vec![oid], fork_base: Some(parent.id.to_string()), n: 1 })
}

#[tokio::test]
async fn real_same_tree_push_advances_one_global_certificate_and_n0_replay_stays_fixed() {
    let (_temp, storage, tip, path) = native_fixture().await;
    let mono = storage.mono_storage();
    let before = mono.get_main_ref("/").await.unwrap().unwrap();
    let (new, payload) = save_same_tree_commit(&storage, &tip).await;
    let id = wh03_enqueue_push(&storage, &path, &tip.id.to_string(), &new, &payload).await;
    assert!(matches!(wh03_exec(&storage,id).await, ExecuteOutcome::Done { root_cas_writes:1,.. }));
    let after = mono.get_main_ref("/").await.unwrap().unwrap();
    assert_eq!((after.ref_commit_hash,after.ref_tree_hash),(before.ref_commit_hash.clone(),before.ref_tree_hash.clone()));
    let head = mono.read_native_publication_head(NATIVE_INSTANCE).await.unwrap();
    assert_eq!(head.token.sequence,1);
    let receipt = mst2_publication::Entity::find().filter(mst2_publication::Column::OperationId.eq(format!("mst2:trunk-queue:{id}")))
        .one(mono.get_connection()).await.unwrap().unwrap();
    assert_eq!(receipt.namespace,path);
    assert_eq!(receipt.new_oid,new);
    assert_eq!(receipt.native_certificate_version,Some(1));
    assert_ne!(receipt.new_oid,head.root.commit);
    let noop = PushPayload { commits:vec![],fork_base:None,n:0 };
    let noop_id = wh03_enqueue_push(&storage,&path,&new,&new,&noop).await;
    assert!(matches!(wh03_exec(&storage,noop_id).await, ExecuteOutcome::Done { root_cas_writes:1,.. }));
    assert_eq!(mono.read_native_publication_head(NATIVE_INSTANCE).await.unwrap().token,head.token);
    mono.get_connection().execute_raw(Statement::from_sql_and_values(mono.get_connection().get_database_backend(),
        "UPDATE push_queue SET status='Running',landed_commit_id=NULL WHERE id=$1",[id.into()],
    )).await.unwrap();
    assert!(matches!(wh03_exec(&storage,id).await, ExecuteOutcome::Done { root_cas_writes:0,.. }));
    assert_eq!(mono.read_native_publication_head(NATIVE_INSTANCE).await.unwrap().token,head.token);
    assert_eq!(mst2_native_publication::Entity::find().count(mono.get_connection()).await.unwrap(),1);
}

#[tokio::test]
async fn real_merge_fails_closed_when_native_publication_is_enabled() {
    let (_temp, storage, tip, path) = native_fixture().await;
    let mono = storage.mono_storage();
    let root_before = mono.get_main_ref("/").await.unwrap().unwrap();
    let head_before = mst2_native_head::Entity::find_by_id("/")
        .one(mono.get_connection())
        .await
        .unwrap()
        .unwrap();

    let cl_link = format!("CL-NATIVE-GUARD-{}", uuid::Uuid::new_v4().simple());
    let EnqueueOutcome::Inserted { id } = storage
        .push_queue_service
        .enqueue(EnqueueRequest {
            kind: PushQueueKindEnum::Merge,
            operation_id: merge_operation_id(&cl_link),
            path: path.clone(),
            old_id: tip.id.to_string(),
            new_id: tip.id.to_string(),
            requester: Some("native-guard-test".to_owned()),
            payload: serde_json::json!({
                "cl_link": cl_link,
                "authz_principal": "native-guard-test",
                "execution_actor": "native-guard-test"
            }),
            ref_name: None,
            is_delete: false,
        })
        .await
        .unwrap()
    else {
        panic!("merge enqueue did not insert");
    };
    assert_eq!(
        storage
            .push_queue_service
            .storage()
            .claim_for_execution(id)
            .await
            .unwrap(),
        ClaimOutcome::Claimed
    );

    let merge_ctx = MergeExecContext {
        storage: storage.clone(),
        git_object_cache: Arc::new(crate::ceres::api_service::cache::GitObjectCache {
            connection: crate::jupiter::tests::test_redis_manager().await,
            prefix: String::new(),
        }),
        abort_before_cl_status: false,
        pause_after_apply: Duration::ZERO,
        pause_after_apply_barrier: None,
    };
    let outcome = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id,
                ..Default::default()
            },
            None,
            Some(&merge_ctx),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ExecuteOutcome::Failed {
            ref failure,
            ref message,
            ..
        } if failure == "Conflict"
            && message == "MST2 native publication does not cover merge writer"
    ));
    assert_eq!(wh03_row_status(&storage, id).await, PushQueueStatusEnum::Failed);

    let root_after = mono.get_main_ref("/").await.unwrap().unwrap();
    assert_eq!(root_after.ref_commit_hash, root_before.ref_commit_hash);
    assert_eq!(root_after.ref_tree_hash, root_before.ref_tree_hash);
    let head_after = mst2_native_head::Entity::find_by_id("/")
        .one(mono.get_connection())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head_after, head_before);
    assert_eq!(mst2_publication::Entity::find().count(mono.get_connection()).await.unwrap(), 0);
    assert_eq!(
        mst2_native_publication::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_publication_outbox::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn real_push_rejects_same_root_changed_publication_token_before_any_ref_write() {
    let (_temp, storage, tip, path) = native_fixture().await;
    let (new,payload)=save_same_tree_commit(&storage,&tip).await;
    let id=wh03_enqueue_push(&storage,&path,&tip.id.to_string(),&new,&payload).await;
    let mono=storage.mono_storage();
    // A separate writer transaction changes only the publication generation.
    mono.get_connection().execute_unprepared("UPDATE mst2_native_head SET sequence=1 WHERE namespace='/'").await.unwrap();
    let outcome=wh03_exec(&storage,id).await;
    assert!(matches!(outcome,ExecuteOutcome::Failed { ref failure,ref message,.. }
        if failure=="Conflict" && message.contains("publication token is stale")));
    assert_eq!(mono.get_main_ref(&path).await.unwrap().unwrap().ref_commit_hash,tip.id.to_string());
    assert_eq!(mst2_native_publication::Entity::find().count(mono.get_connection()).await.unwrap(),0);
    assert_eq!(mst2_publication::Entity::find().count(mono.get_connection()).await.unwrap(),0);
}

async fn api_state(storage: crate::jupiter::storage::Storage) -> crate::api::MonoApiServiceState {
    crate::api::MonoApiServiceState {
        entity_store: storage.entity_store.clone(),storage,
        session_store:crate::api::oauth::api_store::BrowserSessionStore::Anonymous,
        git_object_cache:Arc::new(crate::ceres::api_service::cache::GitObjectCache {
            connection:crate::jupiter::tests::test_redis_manager().await,prefix:String::new(),
        }),listen_addr:"127.0.0.1:0".to_owned(),
    }
}

async fn http_resolve(state: crate::api::MonoApiServiceState,scope:&str) -> (u16,serde_json::Value) {
    use tower::ServiceExt;
    let app=crate::api::router::snapshot_router::routers(state.clone()).with_state(state);
    let response=app.oneshot(axum::http::Request::builder().method("POST").uri("/snapshots/resolve")
        .header("content-type","application/json").body(axum::body::Body::from(serde_json::json!({"target":{"kind":"latest"},"scope":scope}).to_string())).unwrap()).await.unwrap();
    let status=response.status().as_u16();
    let body=axum::body::to_bytes(response.into_body(),1_048_576).await.unwrap();
    (status,serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn http_resolve_captured_before_commit_never_mixes_new_sequence_with_old_descriptor() {
    let (_temp,storage,tip,path)=native_fixture().await;
    let (first,payload)=save_same_tree_commit(&storage,&tip).await;
    let id=wh03_enqueue_push(&storage,&path,&tip.id.to_string(),&first,&payload).await;
    assert!(matches!(wh03_exec(&storage,id).await,ExecuteOutcome::Done{..}));
    let state=api_state(storage.clone()).await;
    let (status,v1)=http_resolve(state.clone(),"/").await;
    assert_eq!(status,200);
    let (_,subscope)=http_resolve(state.clone(),&path).await;
    assert_eq!(v1["publication_sequence"],subscope["publication_sequence"]);
    assert_eq!(v1["writer_epoch"],subscope["writer_epoch"]);
    let root_v1=storage.mono_storage().get_main_ref("/").await.unwrap().unwrap();
    let _ = root_v1;
    let captured=Arc::new(tokio::sync::Barrier::new(2));
    let release=Arc::new(tokio::sync::Barrier::new(2));
    let task_state=state.clone();let task_captured=captured.clone();let task_release=release.clone();
    let mut reader=tokio::spawn(async move {
        crate::api::router::snapshot_router::with_native_resolve_barriers(task_captured,task_release,http_resolve(task_state,"/")).await
    });
    tokio::time::timeout(Duration::from_secs(5),captured.wait()).await.unwrap();
    let first_commit=storage.mono_storage().get_commit_by_hash(&first).await.unwrap().unwrap();
    let first_commit=git_internal::internal::object::commit::Commit::from_mega_model(first_commit);
    let (next,next_payload)=wh03_save_n1_commit(&storage,first_commit.id,"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","next projection").await;
    let id=wh03_enqueue_push(&storage,&path,&first,&next,&next_payload).await;
    assert!(matches!(wh03_exec(&storage,id).await,ExecuteOutcome::Done{..}));
    tokio::time::timeout(Duration::from_secs(5),release.wait()).await.unwrap();
    let (status,delayed)=tokio::time::timeout(Duration::from_secs(10),&mut reader).await.unwrap().unwrap();
    assert_eq!(status,200);assert_eq!(delayed["descriptor"],v1["descriptor"]);
    assert_eq!(delayed["publication_sequence"],v1["publication_sequence"]);
    let (status,v2)=http_resolve(state,"/").await;
    assert_eq!(status,200);assert_eq!(v2["publication_sequence"],"2");
    assert_ne!(v2["descriptor"]["snapshot_id"],v1["descriptor"]["snapshot_id"]);
}
