//! UN-39: baseline pointer codec and version-file rules.

use std::{fs, os::unix::fs::PermissionsExt};

use crate::contract::policy::{
    baseline_pointer::{
        BaselinePointer, encode_pointer, parse_pointer, read_current_pointer,
        validate_artifact_digest, version_file_name, write_current_pointer,
        write_version_for_digest,
    },
    secure_artifact::{ArtifactError, BASELINES_DIR, POINTER_NAME, RestrictedRoot},
    secure_sweep::MaintenanceLock,
};

fn root() -> (tempfile::TempDir, RestrictedRoot) {
    let temp = tempfile::tempdir().expect("temp");
    let root = RestrictedRoot::open(temp.path()).expect("open");
    (temp, root)
}

fn digest(hex64: &str) -> String {
    assert_eq!(hex64.len(), 64);
    format!("sha256:{hex64}")
}

#[test]
fn un39_codec_canonical_bytes_are_exact() {
    let d = digest("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let pointer = BaselinePointer::new(&d).expect("new");
    let bytes = encode_pointer(&pointer).expect("encode");
    assert_eq!(
        bytes,
        br#"{"schema_version":1,"digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
    );
    assert!(!bytes.ends_with(b"\n"));
    assert_eq!(parse_pointer(&bytes).expect("round-trip"), pointer);
}

#[test]
fn un39_codec_rejects_extra_fields_unknown_schema_and_bad_digest() {
    assert!(matches!(
        parse_pointer(
            br#"{"schema_version":1,"digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","extra":true}"#
        ),
        Err(ArtifactError::PointerInvalid { .. })
    ));
    assert!(matches!(
        parse_pointer(
            br#"{"schema_version":2,"digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
        ),
        Err(ArtifactError::PointerInvalid { .. })
    ));
    assert!(matches!(
        validate_artifact_digest("sha256:AAAA"),
        Err(ArtifactError::PointerInvalid { .. })
    ));
    assert!(matches!(
        validate_artifact_digest(
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        ),
        Err(ArtifactError::PointerInvalid { .. })
    ));
    assert!(matches!(
        validate_artifact_digest(
            "md5:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ),
        Err(ArtifactError::PointerInvalid { .. })
    ));
}

#[test]
fn un39_codec_symlinked_pointer_is_refused() {
    let (temp, root) = root();
    let baselines = temp.path().join(BASELINES_DIR);
    fs::create_dir_all(&baselines).expect("mkdir");
    let target = temp.path().join("elsewhere.json");
    fs::write(&target, b"{}").expect("target");
    std::os::unix::fs::symlink(&target, baselines.join(POINTER_NAME)).expect("symlink");

    let err = read_current_pointer(&root).expect_err("symlink");
    assert!(matches!(err, ArtifactError::Io { .. }), "{err}");
}

#[test]
fn un39_codec_version_immutable_and_idempotent() {
    let (_temp, root) = root();
    let d = digest("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let name = version_file_name(&d).expect("name");
    assert_eq!(
        name,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.json"
    );

    let body = br#"{"artifact":"v1"}"#;
    write_version_for_digest(&root, &d, body).expect("first");
    write_version_for_digest(&root, &d, body).expect("idempotent");
    let err = write_version_for_digest(&root, &d, br#"{"artifact":"v2"}"#).expect_err("conflict");
    assert!(matches!(err, ArtifactError::BaselineVersionConflict { .. }));
}

#[test]
fn un39_codec_pointer_write_is_atomic_replace_under_lock() {
    let (temp, root) = root();
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let d1 = digest("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
    let d2 = digest("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");

    write_current_pointer(&root, &lock, &d1).expect("write");
    let path = temp.path().join(BASELINES_DIR).join(POINTER_NAME);
    let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        fs::read(&path).expect("bytes"),
        encode_pointer(&BaselinePointer::new(&d1).unwrap()).unwrap()
    );

    write_current_pointer(&root, &lock, &d2).expect("replace");
    let loaded = read_current_pointer(&root).expect("read").expect("some");
    assert_eq!(loaded.digest, d2);
    assert_eq!(loaded.schema_version, 1);

    // A lock from another root must not authorize this write.
    let other = tempfile::tempdir().expect("other");
    let other_root = RestrictedRoot::open(other.path()).expect("open other");
    let foreign = MaintenanceLock::acquire(&other_root).expect("foreign lock");
    let err = write_current_pointer(&root, &foreign, &d1).expect_err("wrong root");
    assert!(matches!(err, ArtifactError::Io { .. }), "{err}");
}
