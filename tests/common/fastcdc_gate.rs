//! Helpers for the FC-15 mega2 ↔ Libra FastCDC process gate.
//!
//! Path-included only by `integration_fastcdc_libra.rs` so other black-box
//! targets do not compile these symbols.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

/// Pinned Libra commit recorded as `LIBRA_INTEROP_REV` after LB-01.
pub const DEFAULT_LIBRA_INTEROP_REV: &str = "d1aafb23dccb77408ac43786b173f1c9a0d760aa";

pub fn expected_libra_rev() -> String {
    std::env::var("LIBRA_INTEROP_REV").unwrap_or_else(|_| DEFAULT_LIBRA_INTEROP_REV.to_string())
}

/// Replace one-time tokens and absolute LFS URLs before printing child output.
pub fn redact_secrets(raw: &str, token: &str, lfs_url: &str) -> String {
    let mut out = raw.replace(token, "<redacted-token>");
    out = out.replace("other-user-not-in-ready-file", "<redacted-other-token>");
    if !lfs_url.is_empty() {
        out = out.replace(lfs_url, "<redacted-lfs-url>");
        let trimmed = lfs_url.trim_end_matches('/');
        if trimmed != lfs_url {
            out = out.replace(trimmed, "<redacted-lfs-url>");
        }
    }
    out
}

pub struct ReadyFile {
    pub path: PathBuf,
}

impl ReadyFile {
    pub fn write(dir: &Path, lfs_url: &str, token: &str) -> Self {
        assert!(
            lfs_url.ends_with("/info/lfs/"),
            "ready-file lfs_url must be <repo>.git/info/lfs/ with a trailing slash"
        );
        let path = dir.join("mega2-fastcdc-ready.json");
        let body = serde_json::json!({
            "lfs_url": lfs_url,
            "token": token,
            "expected_route": format!("{lfs_url}libra/media/v1"),
        });
        fs::write(
            &path,
            serde_json::to_vec_pretty(&body).expect("ready-file json"),
        )
        .unwrap_or_else(|err| panic!("write ready-file {}: {err}", path.display()));
        let mut perms = fs::metadata(&path)
            .expect("ready-file metadata")
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms).expect("ready-file 0600");
        Self { path }
    }
}

impl Drop for ReadyFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn require_cargo() {
    let status = Command::new("cargo")
        .arg("--version")
        .output()
        .unwrap_or_else(|err| panic!("cargo is required to run the Libra FastCDC child: {err}"));
    assert!(
        status.status.success(),
        "cargo --version failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// Fail closed when `LIBRA_DIR` is missing, dirty, or not the LB-01 revision.
pub fn require_libra_checkout() -> PathBuf {
    require_cargo();
    let dir = std::env::var("LIBRA_DIR").unwrap_or_else(|_| {
        panic!("LIBRA_DIR is required (clean Libra checkout at LIBRA_INTEROP_REV)")
    });
    let dir = PathBuf::from(dir);
    assert!(
        dir.is_dir(),
        "LIBRA_DIR {} is not a readable directory",
        dir.display()
    );
    let test_src = dir.join("tests/media_fastcdc_test.rs");
    let src = fs::read_to_string(&test_src).unwrap_or_else(|err| {
        panic!(
            "LIBRA_DIR is missing LB-01 tests/media_fastcdc_test.rs ({}): {err}",
            test_src.display()
        )
    });
    assert!(
        src.contains("fn mega2_fastcdc_http_interop"),
        "LIBRA_DIR {} does not contain LB-01 test mega2_fastcdc_http_interop",
        dir.display()
    );

    let expected = expected_libra_rev();
    let head = libra_capture(&dir, &["rev-parse", "HEAD"]);
    let head = head.trim();
    assert_eq!(
        head, expected,
        "LIBRA_DIR HEAD {head} != LIBRA_INTEROP_REV {expected}"
    );

    let status = libra_capture(&dir, &["status"]);
    assert!(
        status.contains("working tree clean"),
        "LIBRA_DIR must be a clean checkout; libra status was:\n{status}"
    );
    dir
}

fn libra_capture(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("libra")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to run libra {args:?} in {}: {err}", dir.display()));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "libra {args:?} failed in {} ({})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        dir.display(),
        output.status
    );
    stdout
}
