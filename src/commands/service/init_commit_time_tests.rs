use std::{collections::BTreeMap, sync::Arc};

use git_internal::{
    hash::{HashKind, get_hash_kind, set_hash_kind_for_test},
    internal::object::{ObjectTrait, commit::Commit},
};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, QueryOrder, Set};
use serde_json::{Value, json};

use super::{cli, exec};
use crate::{
    callisto::{mega_blob, mega_commit, mega_tree},
    ceres::pack::materialize::{lock_materialize_tests, materialize_path_refs},
    config::{Config, MonoConfig, MonoObjectFormat, testing::isolated_config},
    jupiter::{
        storage::{Storage, base_storage::StorageConnector, init::database_connection},
        tests::{TestSchemaGuard, test_db_config},
        utils::converter::{BootstrapCommitTime, FromMegaModel, MegaModelConverter},
    },
};

#[test]
fn commit_time_cli_is_explicit_bounded_unsigned_unix_seconds() {
    let default = cli().try_get_matches_from(["init", "--yes"]).unwrap();
    assert!(
        default
            .get_one::<BootstrapCommitTime>("commit-time")
            .is_none()
    );
    for value in ["0", "1790000000", "4294967295"] {
        let args = cli()
            .try_get_matches_from(["init", "--yes", "--commit-time", value])
            .unwrap();
        assert_eq!(
            args.get_one::<BootstrapCommitTime>("commit-time"),
            Some(&value.parse().unwrap())
        );
    }
    for value in [
        "",
        "-1",
        "+1",
        "1.0",
        "1e3",
        " 1",
        "1 ",
        "4294967296",
        "18446744073709551616",
    ] {
        assert!(
            cli()
                .try_get_matches_from(["init", "--yes", &format!("--commit-time={value}")])
                .is_err(),
            "invalid commit time accepted: {value:?}"
        );
        assert!(value.parse::<BootstrapCommitTime>().is_err());
    }
    assert!(
        cli()
            .try_get_matches_from(["init", "--commit-time", "1"])
            .is_err()
    );
    assert!(
        cli()
            .try_get_matches_from(["init", "--yes", "--commit-time", "1", "--commit-time", "2"])
            .is_err()
    );
}

#[test]
fn explicit_commit_time_reproduces_each_hash_format_and_restores_scope() {
    let _scope = set_hash_kind_for_test(HashKind::Sha1);
    for (format, kind) in [
        (MonoObjectFormat::Sha1, HashKind::Sha1),
        (MonoObjectFormat::Sha256, HashKind::Sha256),
        (MonoObjectFormat::Blake3, HashKind::Blake3),
    ] {
        let config = MonoConfig {
            object_format: format,
            ..Default::default()
        };
        for seconds in ["0", "1790000000", "4294967295"] {
            let time = seconds.parse().unwrap();
            let first = MegaModelConverter::init_with_commit_time(&config, Some(time)).unwrap();
            let second = MegaModelConverter::init_with_commit_time(&config, Some(time)).unwrap();
            assert_eq!(first.commit.id.kind(), kind);
            assert_eq!(first.root_tree.id.kind(), kind);
            assert_eq!(first.commit.id, second.commit.id);
            assert_eq!(
                first.commit.to_data().unwrap(),
                second.commit.to_data().unwrap()
            );
            assert_eq!(
                first.root_tree.to_data().unwrap(),
                second.root_tree.to_data().unwrap()
            );
            assert_eq!(first.commit.author.timestamp.to_string(), seconds);
            assert_eq!(first.commit.committer.timestamp.to_string(), seconds);
            assert_eq!(get_hash_kind(), HashKind::Sha1);
        }
    }
}

struct Fixture {
    config: Config,
    storage: Storage,
    schema: TestSchemaGuard,
    _directory: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let (database, schema) = test_db_config(directory.path()).await;
    let mut config = isolated_config(directory.path().join("config"));
    config.database = database;
    config.monorepo.root_dirs = vec!["bench".to_string(), "third-party".to_string()];
    config.redis.url = "redis://127.0.0.1:1".to_string();
    let args = cli()
        .try_get_matches_from(["init", "--yes", "--commit-time", "1790000000"])
        .unwrap();
    exec(config.clone(), &args).await.unwrap();
    let connection = Arc::new(database_connection(&config.database).await.unwrap());
    let object_store =
        crate::jupiter::storage::object_storage::build_object_storage(&config.object_storage)
            .await
            .unwrap();
    let storage = Storage::new_with_connection(Arc::new(config.clone()), connection, object_store)
        .await
        .unwrap();
    Fixture {
        config,
        storage,
        schema,
        _directory: directory,
    }
}

async fn graph_identity(storage: &Storage) -> Value {
    let mono = storage.mono_storage();
    let db = mono.get_connection();
    let commits = mega_commit::Entity::find()
        .order_by_asc(mega_commit::Column::CommitId)
        .all(db)
        .await
        .unwrap();
    let trees = mega_tree::Entity::find().all(db).await.unwrap();
    let blobs = mega_blob::Entity::find().all(db).await.unwrap();
    let mut raw_blobs = BTreeMap::new();
    for blob in blobs {
        raw_blobs.insert(
            blob.blob_id.clone(),
            storage
                .git_service
                .get_object_as_bytes(&blob.blob_id)
                .await
                .unwrap(),
        );
    }
    let refs = mono.get_all_refs("/", false).await.unwrap();
    let path_refs = mono.get_all_refs("/bench", false).await.unwrap();
    json!({
        "commits": commits.into_iter().map(|commit| json!({
            "id": commit.commit_id, "tree": commit.tree, "parents": commit.parents_id,
            "author": commit.author, "committer": commit.committer, "message": commit.content,
        })).collect::<Vec<_>>(),
        "trees": trees.into_iter().map(|tree| (tree.tree_id, tree.sub_trees)).collect::<BTreeMap<_, _>>(),
        "blobs": raw_blobs,
        "root_refs": refs.into_iter().map(|r| (r.ref_name, (r.ref_commit_hash, r.ref_tree_hash))).collect::<BTreeMap<_, _>>(),
        "path_refs": path_refs.into_iter().map(|r| (r.ref_name, (r.ref_commit_hash, r.ref_tree_hash))).collect::<BTreeMap<_, _>>(),
    })
}

#[tokio::test]
async fn cli_bootstrap_two_real_schemas_share_seed_root_tree_and_path_parent() {
    let _lock = lock_materialize_tests().await;
    let first = fixture().await;
    let second = fixture().await;
    assert_ne!(first.schema.schema(), second.schema.schema());
    let first_root = first
        .storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    let second_root = second
        .storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_root.ref_commit_hash, second_root.ref_commit_hash);
    assert_eq!(first_root.ref_tree_hash, second_root.ref_tree_hash);
    let mut seeds = Vec::new();
    for fixture in [&first, &second] {
        let refs = materialize_path_refs(&fixture.storage, "/bench")
            .await
            .unwrap();
        assert_eq!(refs.len(), 1);
        let parent = fixture
            .storage
            .mono_storage()
            .get_commit_by_hash(&refs[0].ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        let root = fixture
            .storage
            .mono_storage()
            .get_commit_by_hash(&first_root.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent.author, root.author);
        assert_eq!(parent.committer, root.committer);
        assert_eq!(parent.parents_id, json!([]));
        let parent = Commit::from_mega_model(parent);
        let seed = Commit::new_with_kind(
            HashKind::Sha1,
            parent.author.clone(),
            parent.committer.clone(),
            parent.tree_id,
            vec![parent.id],
            "\npaired seed",
        )
        .unwrap();
        assert_eq!(seed.parent_commit_ids, vec![parent.id]);
        seeds.push(seed.to_data().unwrap());
    }
    assert_eq!(seeds[0], seeds[1]);
    assert_eq!(
        graph_identity(&first.storage).await,
        graph_identity(&second.storage).await
    );
}

#[tokio::test]
async fn cli_bootstrap_existing_root_replays_exactly_and_rejects_changed_inputs_without_writes() {
    let fixture = fixture().await;
    let before = graph_identity(&fixture.storage).await;
    let args = cli()
        .try_get_matches_from(["init", "--yes", "--commit-time", "1790000000"])
        .unwrap();
    exec(fixture.config.clone(), &args).await.unwrap();
    assert_eq!(graph_identity(&fixture.storage).await, before);
    let changed_time = cli()
        .try_get_matches_from(["init", "--yes", "--commit-time", "1790000001"])
        .unwrap();
    let error = exec(fixture.config.clone(), &changed_time)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("does not match"));
    assert_eq!(graph_identity(&fixture.storage).await, before);
    let mut changed_config = fixture.config.clone();
    changed_config
        .monorepo
        .root_dirs
        .push("different".to_string());
    let error = exec(changed_config, &args).await.unwrap_err();
    assert!(error.to_string().contains("does not match"));
    assert_eq!(graph_identity(&fixture.storage).await, before);
    let default_args = cli().try_get_matches_from(["init", "--yes"]).unwrap();
    exec(fixture.config.clone(), &default_args).await.unwrap();
    assert_eq!(graph_identity(&fixture.storage).await, before);
}

#[tokio::test]
async fn cli_bootstrap_checks_stored_root_objects_even_when_ref_ids_match() {
    let fixture = fixture().await;
    let mono = fixture.storage.mono_storage();
    let root = mono.get_main_ref("/").await.unwrap().unwrap();
    let args = cli()
        .try_get_matches_from(["init", "--yes", "--commit-time", "1790000000"])
        .unwrap();
    let commit = mono
        .get_commit_by_hash(&root.ref_commit_hash)
        .await
        .unwrap()
        .unwrap();
    let original_message = commit.content.clone();
    let mut changed = commit.into_active_model();
    changed.content = Set(Some("\nchanged stored commit".to_string()));
    let changed = changed.update(mono.get_connection()).await.unwrap();
    let corrupt_commit = graph_identity(&fixture.storage).await;
    assert!(
        exec(fixture.config.clone(), &args)
            .await
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
    assert_eq!(graph_identity(&fixture.storage).await, corrupt_commit);
    let mut restored = changed.into_active_model();
    restored.content = Set(original_message);
    restored.update(mono.get_connection()).await.unwrap();
    let tree = mono
        .get_tree_by_hash(&root.ref_tree_hash)
        .await
        .unwrap()
        .unwrap();
    let mut bytes = tree.sub_trees.clone();
    bytes.push(b'x');
    let mut changed_tree = tree.into_active_model();
    changed_tree.sub_trees = Set(bytes);
    changed_tree.update(mono.get_connection()).await.unwrap();
    let corrupt_tree = graph_identity(&fixture.storage).await;
    assert!(
        exec(fixture.config.clone(), &args)
            .await
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
    assert_eq!(graph_identity(&fixture.storage).await, corrupt_tree);
}
