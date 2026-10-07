use bytes::Bytes;

use crate::callisto::{
    mst2_native_head, mst2_native_publication, mst2_publication, mst2_publication_outbox,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter, Statement};
use crate::jupiter::utils::converter::FromMegaModel;

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
    use git_internal::internal::object::{
        commit::Commit,
        tree::{Tree, TreeItem, TreeItemMode},
    };
    let keep_oid = storage.git_service.save_object_from_raw(Bytes::from_static(b"keep")).await.unwrap();
    let old_oid = storage.git_service.save_object_from_raw(Bytes::from_static(b"old")).await.unwrap();
    let child = Tree::from_tree_items(vec![wh03_blob_item("x.txt", &old_oid)]).unwrap();
    let root_tree = Tree::from_tree_items(vec![
        wh03_blob_item(".gitkeep", &keep_oid),
        TreeItem::new(TreeItemMode::Tree, child.id, name.clone()),
    ]).unwrap();
    let root_commit = Commit::from_tree_id(root_tree.id, vec![], "root");
    let tip = Commit::from_tree_id(child.id, vec![], "path tip");
    let mono = storage.mono_storage();
    mono.save_mega_trees(vec![child.clone(), root_tree.clone()], root_commit.id, None).await.unwrap();
    mono.save_mega_commits(vec![root_commit.clone(), tip.clone()], None).await.unwrap();
    mono.save_refs(mega_refs::Model::new(
        "/", MEGA_BRANCH_NAME.to_owned(), root_commit.id.to_string(), root_tree.id.to_string(), false,
    ), None).await.unwrap();
    let path = format!("/{name}");
    mono.save_refs(mega_refs::Model::new(
        path.clone(), MEGA_BRANCH_NAME.to_owned(), tip.id.to_string(), child.id.to_string(), false,
    ), None).await.unwrap();
    storage.push_queue_service.push_queue_storage.set_control_flags(Some(true), None, None).await.unwrap();
    mono.initialize_native_publication_for_maintenance(
        NATIVE_INSTANCE,
        &crate::jupiter::storage::native_publication_storage::NativeRoot {
            commit: root_commit.id.to_string(), tree: root_tree.id.to_string(),
        },
    ).await.unwrap();
    assert!(mono.read_native_publication_head(NATIVE_INSTANCE).await.is_err());
    storage.push_queue_service.push_queue_storage.set_control_flags(Some(false), None, None).await.unwrap();
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
async fn maintenance_cannot_reinitialize_published_history_after_head_loss() {
    let (_temp, storage, tip, path) = native_fixture().await;
    let mono = storage.mono_storage();
    let (new, payload) = save_same_tree_commit(&storage, &tip).await;
    let id = wh03_enqueue_push(&storage, &path, &tip.id.to_string(), &new, &payload).await;
    assert!(matches!(wh03_exec(&storage, id).await, ExecuteOutcome::Done { .. }));
    let head = mono.read_native_publication_head(NATIVE_INSTANCE).await.unwrap();
    let certificates = mst2_native_publication::Entity::find().all(mono.get_connection()).await.unwrap();
    let receipts = mst2_publication::Entity::find().all(mono.get_connection()).await.unwrap();
    let outbox = mst2_publication_outbox::Entity::find().all(mono.get_connection()).await.unwrap();
    assert_eq!(certificates.len(), 1);
    storage.push_queue_service.push_queue_storage.set_control_flags(Some(true), None, None).await.unwrap();
    mono.get_connection().execute_unprepared("DELETE FROM mst2_native_head").await.unwrap();
    for instance in [NATIVE_INSTANCE, "11111111-2222-4333-8444-555555555555"] {
        let error = mono.initialize_native_publication_for_maintenance(instance, &head.root).await.unwrap_err();
        assert!(error.to_string().contains("native publication history exists"), "rejected initialization for {instance}: {error}");
        let released = mono.get_connection().begin().await.unwrap();
        assert!(PushQueueStorage::try_mono_write_lock(&released).await.unwrap(), "rejected initialization must release the mono write lock before returning");
        released.rollback().await.unwrap();
    }
    assert_eq!(mst2_native_head::Entity::find().count(mono.get_connection()).await.unwrap(), 0);
    assert_eq!(mst2_native_publication::Entity::find().all(mono.get_connection()).await.unwrap(), certificates);
    assert_eq!(mst2_publication::Entity::find().all(mono.get_connection()).await.unwrap(), receipts);
    assert_eq!(mst2_publication_outbox::Entity::find().all(mono.get_connection()).await.unwrap(), outbox);
    assert!(storage.push_queue_service.push_queue_storage.get_control().await.unwrap().paused);
    mono.get_connection().execute_unprepared("DELETE FROM mst2_native_publication").await.unwrap();
    for instance in [NATIVE_INSTANCE, "11111111-2222-4333-8444-555555555555"] {
        let error = mono.initialize_native_publication_for_maintenance(instance, &head.root).await.unwrap_err();
        assert!(error.to_string().contains("native publication history exists"), "rejected initialization for {instance}: {error}");
        let released = mono.get_connection().begin().await.unwrap();
        assert!(PushQueueStorage::try_mono_write_lock(&released).await.unwrap(), "rejected initialization must release the mono write lock before returning");
        released.rollback().await.unwrap();
    }
    assert_eq!(mst2_native_head::Entity::find().count(mono.get_connection()).await.unwrap(), 0);
    assert_eq!(mst2_native_publication::Entity::find().count(mono.get_connection()).await.unwrap(), 0);
    assert_eq!(mst2_publication::Entity::find().all(mono.get_connection()).await.unwrap(), receipts);
    assert_eq!(mst2_publication_outbox::Entity::find().all(mono.get_connection()).await.unwrap(), outbox);
    assert!(storage.push_queue_service.push_queue_storage.get_control().await.unwrap().paused);
    let root = mono.get_main_ref("/").await.unwrap().unwrap();
    assert_eq!((root.ref_commit_hash, root.ref_tree_hash), (head.root.commit, head.root.tree));
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
    #[cfg(unix)]
    let writer=crate::ceres::snapshot::projection_writer::ProjectionObservationSink::start(_temp.path()).unwrap();
    #[cfg(unix)]
    let storage={ let mut storage=storage; storage.projection_observation_sink=Some(writer.clone()); storage };
    let (first,payload)=save_same_tree_commit(&storage,&tip).await;
    let id=wh03_enqueue_push(&storage,&path,&tip.id.to_string(),&first,&payload).await;
    assert!(matches!(wh03_exec(&storage,id).await,ExecuteOutcome::Done{..}));
    let state=api_state(storage.clone()).await;
    let ((status,v1),first_observations)=crate::ceres::snapshot::projection_observation::with_observations(http_resolve(state.clone(),"/")).await;
    assert_eq!(status,200,"{v1}");
    assert_eq!(first_observations.len(),1);
    let first_identity=first_observations[0].test_identity();
    assert_eq!(first_identity["snapshot_id"],v1["descriptor"]["snapshot_id"]);
    assert_eq!(first_identity["metadata_root"],v1["descriptor"]["metadata_root"]);
    assert_eq!(first_identity["namespace_view_id"],v1["descriptor"]["namespace_view_id"]);
    assert_eq!(first_identity["instance_id"],v1["descriptor"]["instance_id"]);
    assert_eq!(first_identity["native_publication_sequence"],v1["publication_sequence"]);
    assert_eq!(first_identity["native_writer_epoch"],v1["writer_epoch"]);
    assert!(first_identity["native_certificate_receipt_id"].as_u64().unwrap()>0);
    assert!(!first_identity["request_id"].as_str().unwrap().is_empty());
    let (status,subscope)=http_resolve(state.clone(),&path).await;
    assert_eq!(status,200,"{subscope}");
    assert_eq!(v1["publication_sequence"],subscope["publication_sequence"]);
    assert_eq!(v1["writer_epoch"],subscope["writer_epoch"]);
    let root_v1=storage.mono_storage().get_main_ref("/").await.unwrap().unwrap();
    assert_eq!(first_identity["root_commit_oid"],git_internal::hash::ObjectHash::from_hex_for_kind(git_internal::hash::get_hash_kind(),&root_v1.ref_commit_hash).unwrap().to_tagged_string());
    assert_eq!(first_identity["root_tree_oid"],git_internal::hash::ObjectHash::from_hex_for_kind(git_internal::hash::get_hash_kind(),&root_v1.ref_tree_hash).unwrap().to_tagged_string());
    let native_v1=storage.mono_storage().read_native_publication_head(storage.config().mst2.instance_uuid.as_deref().unwrap()).await.unwrap();
    assert_eq!(first_identity["native_certificate_receipt_id"].as_u64(),Some(native_v1.token.certificate.unwrap() as u64));
    let captured=Arc::new(tokio::sync::Barrier::new(2));
    let release=Arc::new(tokio::sync::Barrier::new(2));
    let task_state=state.clone();let task_captured=captured.clone();let task_release=release.clone();
    let mut reader=tokio::spawn(async move {
        crate::ceres::snapshot::projection_observation::with_observations(crate::api::router::snapshot_router::with_native_resolve_barriers(task_captured,task_release,http_resolve(task_state,"/"))).await
    });
    tokio::time::timeout(Duration::from_secs(5),captured.wait()).await.unwrap();
    let first_commit=storage.mono_storage().get_commit_by_hash(&first).await.unwrap().unwrap();
    let first_commit=git_internal::internal::object::commit::Commit::from_mega_model(first_commit);
    let next_blob = storage.git_service.save_object_from_raw(Bytes::from_static(b"next projection")).await.unwrap();
    let (next,next_payload)=wh03_save_n1_commit(&storage,first_commit.id,&next_blob,"next projection").await;
    let id=wh03_enqueue_push(&storage,&path,&first,&next,&next_payload).await;
    assert!(matches!(wh03_exec(&storage,id).await,ExecuteOutcome::Done{..}));
    tokio::time::timeout(Duration::from_secs(5),release.wait()).await.unwrap();
    let ((status,delayed),delayed_observations)=tokio::time::timeout(Duration::from_secs(10),&mut reader).await.unwrap().unwrap();
    assert_eq!(status,503,"{delayed}");
    assert_eq!(delayed["error"]["code"],"SNAPSHOT_NOT_READY");
    assert_eq!(delayed["error"]["retryable"],true);
    assert!(delayed.get("descriptor").is_none());
    assert!(delayed.get("publication_sequence").is_none());
    assert!(delayed_observations.is_empty(),"rejected stale handoff emitted a success observation");
    use tower::ServiceExt;
    let old_app=crate::api::router::snapshot_router::routers(state.clone()).with_state(state.clone());
    let old=old_app.oneshot(axum::http::Request::builder().method("GET")
        .uri(format!("/snapshots/{}/descriptor",v1["descriptor"]["snapshot_id"].as_str().unwrap()))
        .header("x-mega-snapshot-lease",v1["lease_id"].as_str().unwrap())
        .body(axum::body::Body::empty()).unwrap()).await.unwrap();
    assert_eq!(old.status(),200);
    let old=axum::body::to_bytes(old.into_body(),1_048_576).await.unwrap();
    let old:serde_json::Value=serde_json::from_slice(&old).unwrap();
    assert_eq!(old["descriptor"],v1["descriptor"],"already protected old SID changed with the writer");
    let ((status,v2),next_observations)=crate::ceres::snapshot::projection_observation::with_observations(http_resolve(state.clone(),"/")).await;
    assert_eq!(status,200,"{v2}");assert_eq!(v2["publication_sequence"],"2");
    assert_ne!(v2["descriptor"]["snapshot_id"],v1["descriptor"]["snapshot_id"]);
    assert_eq!(next_observations.len(),1);
    let next_identity=next_observations[0].test_identity();
    assert_eq!(next_identity["native_publication_sequence"],v2["publication_sequence"]);
    assert_eq!(next_identity["snapshot_id"],v2["descriptor"]["snapshot_id"]);
    assert_eq!(next_identity["metadata_root"],v2["descriptor"]["metadata_root"]);
    assert_eq!(next_identity["namespace_view_id"],v2["descriptor"]["namespace_view_id"]);
    assert_eq!(next_identity["instance_id"],v2["descriptor"]["instance_id"]);
    assert_eq!(next_identity["native_writer_epoch"],v2["writer_epoch"]);
    let native_v2=storage.mono_storage().read_native_publication_head(storage.config().mst2.instance_uuid.as_deref().unwrap()).await.unwrap();
    assert_eq!(next_identity["root_commit_oid"],git_internal::hash::ObjectHash::from_hex_for_kind(git_internal::hash::get_hash_kind(),&native_v2.root.commit).unwrap().to_tagged_string());
    assert_eq!(next_identity["root_tree_oid"],git_internal::hash::ObjectHash::from_hex_for_kind(git_internal::hash::get_hash_kind(),&native_v2.root.tree).unwrap().to_tagged_string());
    assert_eq!(next_identity["native_certificate_receipt_id"].as_u64(),Some(native_v2.token.certificate.unwrap() as u64));
    assert_ne!(next_identity["native_certificate_receipt_id"],first_identity["native_certificate_receipt_id"]);
    let ((status,_),failed_observations)=crate::ceres::snapshot::projection_observation::with_observations(http_resolve(state,"/absent-observation-scope")).await;
    assert_eq!(status,404);
    assert!(failed_observations.is_empty(),"failed projection emitted success observation");
    #[cfg(unix)]
    {
        writer.shutdown(std::time::Instant::now()+Duration::from_secs(5)).await.unwrap();
        let directory=writer.test_directory(_temp.path());
        let records=std::fs::read_to_string(directory.join("records.jsonl")).unwrap();
        let records:Vec<serde_json::Value>=records.lines().map(|line|serde_json::from_str(line).unwrap()).collect();
        assert_eq!(records.len(),3,"failed resolve must not be a successful observation");
        for (record,observation) in [(&records[0],&first_observations[0]),(&records[2],&next_observations[0])] {
            let typed=serde_json::to_value(observation.wire_record()).unwrap();
            assert_eq!(record["payload"],typed,"writer must preserve the full validated operation tuple");
            assert_eq!(record["payload"].as_object().unwrap().len(),38);
        }
        assert_ne!(records[0]["payload"]["request_id"],records[2]["payload"]["request_id"]);
        let status:serde_json::Value=serde_json::from_slice(&std::fs::read(directory.join("status.json")).unwrap()).unwrap();
        assert_eq!(status["closed"],true);
        assert_eq!(status["accepted_records"],3);
        assert_eq!(status["written_records"],3);
        assert_eq!(status["first_error_code"],0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rejected_native_observation_source_does_not_change_ready_resolve_and_is_a_sticky_writer_failure() {
    let (temp,mut storage,tip,path)=native_fixture().await;
    let (first,payload)=save_same_tree_commit(&storage,&tip).await;
    let id=wh03_enqueue_push(&storage,&path,&tip.id.to_string(),&first,&payload).await;
    assert!(matches!(wh03_exec(&storage,id).await,ExecuteOutcome::Done{..}));
    let head=storage.mono_storage().read_native_publication_head(NATIVE_INSTANCE).await.unwrap();
    assert_eq!(head.token.sequence,1);
    assert!(head.token.certificate.unwrap()>0);
    let writer=crate::ceres::snapshot::projection_writer::ProjectionObservationSink::start(temp.path()).unwrap();
    storage.projection_observation_sink=Some(writer.clone());
    let state=api_state(storage).await;
    // Corrupt only the observation's captured certificate. The actual native
    // head is READY; INITIALIZING heads remain correctly rejected with 503.
    let ((status,response),observations)=crate::ceres::snapshot::projection_observation::with_observations(
        crate::api::router::snapshot_router::with_rejected_native_observation_source(http_resolve(state,"/"))
    ).await;
    assert_eq!(status,200,"{response}");
    assert_eq!(response["publication_sequence"],"1");
    assert!(observations.is_empty());
    assert!(writer.shutdown(std::time::Instant::now()+Duration::from_secs(5)).await.is_err());
    let directory=writer.test_directory(temp.path());
    assert!(std::fs::read(directory.join("records.jsonl")).unwrap().is_empty());
    let status:serde_json::Value=serde_json::from_slice(&std::fs::read(directory.join("status.json")).unwrap()).unwrap();
    assert_eq!(status["first_error_code"],crate::ceres::snapshot::projection_writer::WriterFailure::ObservationBindingRejected as u8);
    assert_eq!(status["accepted_records"],0);
    assert_eq!(status["closed"],true);
}
