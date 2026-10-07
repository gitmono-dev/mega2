use super::*;

fn lookup_body(paths: &[&str]) -> Body {
    Body::from(json!({"paths": paths}).to_string())
}

#[tokio::test]
async fn mst2_durable_http_lookup_uses_verified_metadata_after_service_rebuild_without_body_reads()
{
    let fixture = Fixture::new_with_pg_config(true).await;
    let state = rebuilt(&fixture).await;
    assert!(state.storage.native_snapshot_sessions.get().is_none());
    assert!(!Arc::ptr_eq(
        &state.storage.native_projection_cache,
        &fixture.state.storage.native_projection_cache
    ));
    let paths = [
        "/file",
        "/alias",
        "/executable",
        "/empty",
        "/link",
        "/nested/file",
        "/directory",
        "/nested",
        "/missing",
        "/file/child",
        "/link/child",
    ];
    let value = success_json(
        app(&state)
            .oneshot(fixture.request("POST", "lookup", lookup_body(&paths)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(value["snapshot_id"], fixture.snapshot);
    let results = value["results"].as_array().unwrap();
    assert_eq!(results.len(), paths.len());
    for (result, path) in results.iter().zip(paths) {
        assert_eq!(result["path"], path);
    }
    for (index, kind, size, content_digest) in [
        (0, "regular", fixture.raw.len(), fixture.digest_string()),
        (1, "regular", fixture.raw.len(), fixture.digest_string()),
        (2, "executable", fixture.raw.len(), fixture.digest_string()),
        (3, "regular", 0, format!("sha256:{}", hex_of(&digest(&[])))),
        (
            4,
            "symlink",
            4,
            format!("sha256:{}", hex_of(&digest(b"file"))),
        ),
        (5, "regular", fixture.raw.len(), fixture.digest_string()),
    ] {
        assert_eq!(results[index]["status"], "found");
        assert_eq!(
            results[index]["node"],
            json!({
                "fs_kind": kind,
                "name": paths[index].rsplit('/').next().unwrap(),
                "size": size.to_string(),
                "content_digest": content_digest,
            })
        );
    }
    let proofs = value["proof_pages"].as_array().unwrap();
    assert!(!proofs.is_empty());
    let mut proof_digests = std::collections::HashSet::new();
    for proof in proofs {
        let bytes = STANDARD
            .decode(proof["data_base64"].as_str().unwrap())
            .unwrap();
        mst2_codec::metapage::Page::decode(&bytes).unwrap();
        let digest = format!("sha256:{}", hex_of(&mst2_codec::metapage::page_id(&bytes)));
        assert_eq!(proof["digest"], digest);
        assert!(proof_digests.insert(digest));
    }
    for index in [6, 7] {
        let result = &results[index];
        assert_eq!(result["status"], "found");
        assert_eq!(result["node"]["fs_kind"], "directory");
        assert_eq!(
            result["node"]["name"],
            paths[index].rsplit('/').next().unwrap()
        );
        assert_eq!(result["node"]["node_class"], "native_tree");
        assert_eq!(result["node"]["lifecycle"], "mutable");
        assert!(proof_digests.contains(result["node"]["directory_root"].as_str().unwrap()));
    }
    for (index, status) in [
        (8, "absent"),
        (9, "not_directory"),
        (10, "symlink_traversal"),
    ] {
        assert_eq!(
            results[index],
            json!({"path": paths[index], "status": status})
        );
    }
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_lookup_missing_and_noncurrent_facts_never_fall_back_to_body_or_cache() {
    let fixture = Fixture::new_with_pg_config(true).await;
    fixture.map("/file").await;
    let original = fixture.fact().await;
    fixture.counts.reset();
    fixture.delete_fact().await;
    error(
        fixture
            .send("POST", "lookup", lookup_body(&["/file"]))
            .await,
        503,
        "METADATA_NOT_READY",
        true,
    )
    .await;
    fixture.counts.assert(0, 0);
    for (domain, kind, generation) in [
        ("git", "blob", 1),
        ("other", "blob", MST2_VERIFICATION_VERSION),
        ("git", "tree", MST2_VERIFICATION_VERSION),
    ] {
        let mut fact = original.clone();
        fact.storage_domain = domain.to_string();
        fact.object_kind = kind.to_string();
        fact.verification_version = generation;
        fixture.replace_fact(fact).await;
        error(
            fixture
                .send("POST", "lookup", lookup_body(&["/alias"]))
                .await,
            503,
            "METADATA_NOT_READY",
            true,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    fixture.replace_fact(original).await;
    success_json(
        fixture
            .send("POST", "lookup", lookup_body(&["/file"]))
            .await,
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_lookup_invalid_verified_facts_fail_before_body_reads() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let original = fixture.fact().await;
    for case in 0..5 {
        let mut fact = original.clone();
        match case {
            0 => fact.state = "PENDING".to_string(),
            1 => fact.verification_version = MST2_VERIFICATION_VERSION + 1,
            2 => fact.size = -1,
            3 => fact.size = 8_796_093_022_209,
            4 => {
                fact.raw_sha256.pop().unwrap();
            }
            _ => unreachable!(),
        }
        fixture.replace_fact(fact).await;
        error(
            fixture
                .send("POST", "lookup", lookup_body(&["/file"]))
                .await,
            502,
            "INTEGRITY_ERROR",
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    fixture.replace_fact(original).await;
    success_json(
        fixture
            .send("POST", "lookup", lookup_body(&["/file"]))
            .await,
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_lookup_symlink_facts_outside_profile_fail_without_body_reads() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let main = mono.get_main_ref("/").await.unwrap().unwrap();
    let handler = MonoApiService::from(&fixture.state);
    let root = handler.get_tree_by_hash(&main.ref_tree_hash).await.unwrap();
    let MetadataWalkOutcome::FoundFile { oid, .. } =
        resolve_abs_metadata(&handler, &root, "/project/link")
            .await
            .unwrap()
    else {
        panic!("fixture link must be a fixed file");
    };
    let original = mono
        .get_verified_blobs(vec![oid.clone()])
        .await
        .unwrap()
        .remove(&oid)
        .unwrap();
    for size in [0, 4096] {
        let mut fact = original.clone().into_active_model();
        fact.size = Set(size);
        fact.update(mono.get_connection()).await.unwrap();
        error(
            fixture
                .send("POST", "lookup", lookup_body(&["/link"]))
                .await,
            502,
            "INTEGRITY_ERROR",
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    let mut restored = original.into_active_model();
    restored.size = Set(4);
    restored.update(mono.get_connection()).await.unwrap();
    let result = success_json(
        fixture
            .send("POST", "lookup", lookup_body(&["/link"]))
            .await,
    )
    .await;
    assert_eq!(result["results"][0]["node"]["size"], "4");
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_lookup_verified_fact_database_failure_is_typed_without_body_reads() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    mono.get_connection()
        .execute_unprepared(
            "ALTER TABLE mst2_verified_object RENAME TO mst2_verified_object_unavailable",
        )
        .await
        .unwrap();
    error(
        fixture
            .send("POST", "lookup", lookup_body(&["/file"]))
            .await,
        500,
        "INTERNAL",
        true,
    )
    .await;
    fixture.counts.assert(0, 0);
    mono.get_connection()
        .execute_unprepared(
            "ALTER TABLE mst2_verified_object_unavailable RENAME TO mst2_verified_object",
        )
        .await
        .unwrap();
    success_json(
        fixture
            .send("POST", "lookup", lookup_body(&["/file"]))
            .await,
    )
    .await;
    fixture.counts.assert(0, 0);
}
