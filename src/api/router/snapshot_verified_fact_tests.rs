use axum::{body::to_bytes, http::StatusCode};
use serde_json::Value;

use super::FsKind;
use crate::{
    callisto::mst2_verified_object, jupiter::storage::mono_storage::MST2_VERIFICATION_VERSION,
};

const OID: &str = "0123456789abcdef0123456789abcdef01234567";
const PATH: &str = "synthetic/file";
const DIGEST: [u8; 32] = [0xab; 32];

fn verified_fact(size: i64) -> mst2_verified_object::Model {
    mst2_verified_object::Model {
        id: 1,
        storage_domain: "git".to_owned(),
        git_oid: OID.to_owned(),
        object_kind: "blob".to_owned(),
        raw_sha256: DIGEST.to_vec(),
        size,
        verification_version: MST2_VERIFICATION_VERSION,
        state: "VERIFIED".to_owned(),
        created_at: chrono::Utc::now().fixed_offset(),
    }
}

async fn assert_error(
    fs_kind: FsKind,
    fact: mst2_verified_object::Model,
    expected_digest: Option<&str>,
    status: StatusCode,
    code: &str,
    message: &str,
) {
    let error =
        super::metadata_from_verified_fact(fs_kind, OID.to_owned(), PATH, expected_digest, fact)
            .err()
            .expect("invalid verified fact must be rejected");
    let response = super::mst2_error_response(error);
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()["content-type"], "application/json");
    let bytes = to_bytes(response.into_body(), 16 * 1024)
        .await
        .expect("error response body");
    let value: Value = serde_json::from_slice(&bytes).expect("JSON error envelope");
    assert_eq!(value["error"]["code"], code);
    assert_eq!(value["error"]["message"], message);
    assert_eq!(value["error"]["retryable"], false);
    assert!(value["error"]["request_id"].is_string());
}

#[tokio::test]
async fn invalid_digest_lengths_preserve_integrity_error() {
    for length in [0, 31, 33] {
        let mut fact = verified_fact(64);
        fact.raw_sha256 = vec![0xab; length];
        assert_error(
            FsKind::Regular,
            fact,
            None,
            StatusCode::BAD_GATEWAY,
            "INTEGRITY_ERROR",
            "invalid verified content digest",
        )
        .await;
    }
}

#[tokio::test]
async fn negative_size_preserves_integrity_error() {
    assert_error(
        FsKind::Regular,
        verified_fact(-1),
        None,
        StatusCode::BAD_GATEWAY,
        "INTEGRITY_ERROR",
        "invalid verified content size",
    )
    .await;
}

#[tokio::test]
async fn zero_length_symlink_preserves_integrity_error() {
    assert_error(
        FsKind::Symlink,
        verified_fact(0),
        None,
        StatusCode::BAD_GATEWAY,
        "INTEGRITY_ERROR",
        "verified symlink size is outside the fixed filesystem profile",
    )
    .await;
}

#[tokio::test]
async fn oversized_symlink_preserves_integrity_error() {
    assert_error(
        FsKind::Symlink,
        verified_fact(4096),
        None,
        StatusCode::BAD_GATEWAY,
        "INTEGRITY_ERROR",
        "verified symlink size is outside the fixed filesystem profile",
    )
    .await;
}

#[tokio::test]
async fn expected_digest_mismatch_preserves_path_error() {
    assert_error(
        FsKind::Regular,
        verified_fact(64),
        Some("sha256:0000000000000000000000000000000000000000000000000000000000000000"),
        StatusCode::CONFLICT,
        "EXPECTED_DIGEST_MISMATCH",
        "synthetic/file: content does not match expected_digest",
    )
    .await;
}

#[test]
fn valid_facts_preserve_digest_size_and_oid() {
    let expected_digest = format!("sha256:{}", "ab".repeat(32));
    for (fs_kind, size) in [
        (FsKind::Regular, 0_u16),
        (FsKind::Executable, 32),
        (FsKind::Symlink, 1),
        (FsKind::Symlink, 4095),
    ] {
        let actual = super::metadata_from_verified_fact(
            fs_kind,
            OID.to_owned(),
            PATH,
            Some(&expected_digest),
            verified_fact(i64::from(size)),
        )
        .expect("valid verified fact must retain its metadata");
        assert_eq!(actual.digest, DIGEST);
        assert_eq!(actual.size, u64::from(size));
        assert_eq!(actual.oid, OID);
    }
}
