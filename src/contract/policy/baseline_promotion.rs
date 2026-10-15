//! UN-35: baseline promotion CAS under the shared maintenance lock.
//! UN-40: crash-window end states and retry convergence for that CAS.
//!
//! Codec/version rules are UN-39. This module freezes the critical-section
//! order, expect fencing, promotion producer wiring into
//! `admit_and_reserve_locked`, and the W1..W6 retry table.
//!
//! # Crash windows (UN-40)
//!
//! | Window | Visible state | Retry | Temps | Exit |
//! |---|---|---|---|---|
//! | W1 | version `.tmp-*` left; pointer unchanged | normal promote | sweep clears by age (UN-38) | 0 on success |
//! | W2 | version may exist after rename; dir fsync pending | UN-39 idempotent version write | — | 0 |
//! | W3 | version present; pointer still old | continue (fence on old) | — | 0 |
//! | W4 | pointer `.tmp-*` left | same as W1 | sweep by age | 0 |
//! | W5 | pointer may be old or new after rename | read pointer → already-current or continue | — | 0 |
//! | W6 | fully committed before return | already-current | — | 0 |
//!
//! **Already-current (frozen):** pointer digest equals the target **and** the
//! version file bytes match the candidate → success with
//! [`ALREADY_CURRENT_MARKER`] (CLI maps this to exit 0 + stderr), independent of
//! expect fencing flags. `.tmp-*` cleanup stays UN-38's age rule.

use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::contract::policy::{
    baseline_pointer::{
        read_current_pointer, read_version_for_digest, validate_artifact_digest,
        write_current_pointer, write_version_for_digest,
    },
    secure_artifact::{ArtifactError, ArtifactResult, RestrictedRoot},
    secure_hardcap::{new_op_id, rfc3339_utc},
    secure_lifecycle::{
        NoLeases, ReserveRequest, SettleRequest, abort_locked, admit_and_reserve_locked,
        commit_locked,
    },
    secure_producer::Producer,
    secure_sweep::MaintenanceLock,
};

/// Frozen process exit code when fencing rejects a promote (UN-29 table).
pub const PROMOTION_FENCING_EXIT_CODE: i32 = 3;

/// Stderr token for already-current idempotent success (UN-40 / CLI).
pub const ALREADY_CURRENT_MARKER: &str = "already-current";

/// Named crash windows from the UN-40 end-state table (documentary + tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashWindow {
    /// Version temp left; pointer unchanged.
    W1VersionTemp,
    /// Version renamed; directory fsync not yet durable.
    W2VersionRenamed,
    /// Version present; pointer still previous digest / absent.
    W3VersionWithoutPointer,
    /// Pointer temp left.
    W4PointerTemp,
    /// Pointer renamed; directory fsync not yet durable.
    W5PointerRenamed,
    /// Promote returned successfully; caller crashed before observing it.
    W6FullyCommitted,
}

/// Caller-supplied fencing expectation (mutually exclusive, one required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteFence {
    /// First promote: pointer must be absent.
    ExpectNoCurrent,
    /// Update: pointer must currently equal this digest.
    ExpectCurrentDigest(String),
}

/// Inputs for one promote attempt (aside from the held lock / root).
#[derive(Debug, Clone)]
pub struct PromoteRequest<'a> {
    pub candidate: &'a [u8],
    pub expect_digest: &'a str,
    pub fence: PromoteFence,
    pub now: SystemTime,
    pub directory_entries: usize,
}

/// Result of a successful promote critical section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// Version written and pointer switched.
    Promoted { digest: String },
    /// Pointer already named the target digest with matching version bytes.
    AlreadyCurrent { digest: String },
}

impl PromoteOutcome {
    /// CLI/stderr token for [`PromoteOutcome::AlreadyCurrent`].
    pub fn already_current_marker(&self) -> Option<&'static str> {
        match self {
            Self::AlreadyCurrent { .. } => Some(ALREADY_CURRENT_MARKER),
            Self::Promoted { .. } => None,
        }
    }
}

/// Resolve CLI-style expect flags into a single fence value.
pub fn resolve_promote_fence(
    expect_no_current: bool,
    expect_current_digest: Option<&str>,
) -> ArtifactResult<PromoteFence> {
    match (expect_no_current, expect_current_digest) {
        (true, Some(_)) => Err(ArtifactError::PromotionRejected {
            reason: "--expect-no-current and --expect-current-digest are mutually exclusive".into(),
        }),
        (false, None) => Err(ArtifactError::PromotionRejected {
            reason: "one of --expect-no-current or --expect-current-digest is required".into(),
        }),
        (true, None) => Ok(PromoteFence::ExpectNoCurrent),
        (false, Some(digest)) => {
            validate_artifact_digest(digest)?;
            Ok(PromoteFence::ExpectCurrentDigest(digest.to_string()))
        }
    }
}

/// SHA-256 of `bytes` as `sha256:<64 lowercase hex>`.
pub fn content_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Promote under an already-held maintenance lock.
///
/// Critical-section order (frozen): integrity → already-current → fencing →
/// admit → write version → switch pointer → settle.
///
/// Crash recovery is **retry this function** against the durable end state
/// (UN-40 W1..W6); temps are left for UN-38 sweep.
pub fn promote_locked(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: PromoteRequest<'_>,
) -> ArtifactResult<PromoteOutcome> {
    promote_locked_inner(root, lock, request, None)
}

#[cfg(test)]
pub(crate) fn promote_locked_with_between_hook(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: PromoteRequest<'_>,
    between_version_and_pointer: &dyn Fn(),
) -> ArtifactResult<PromoteOutcome> {
    promote_locked_inner(root, lock, request, Some(between_version_and_pointer))
}

fn promote_locked_inner(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: PromoteRequest<'_>,
    between_version_and_pointer: Option<&dyn Fn()>,
) -> ArtifactResult<PromoteOutcome> {
    lock.assert_guards(root)?;
    validate_artifact_digest(request.expect_digest)?;

    // 1. Candidate integrity: recompute digest vs --expect-digest.
    let actual = content_digest(request.candidate);
    if actual != request.expect_digest {
        return Err(ArtifactError::PromotionRejected {
            reason: format!(
                "candidate digest {actual} does not match --expect-digest {}",
                request.expect_digest
            ),
        });
    }
    let target = actual;

    // 2. Already-current / repair (UN-40): pointer == target.
    let mut skip_fencing = false;
    if let Some(current) = read_current_pointer(root)?
        && current.digest == target
    {
        match read_version_for_digest(root, &target)? {
            Some(existing) if existing.as_slice() == request.candidate => {
                // W5/W6 / mid-settle crash: durable promote is done but the
                // reservation may still be open — settle it before returning.
                settle_matching_promotion(
                    root,
                    lock,
                    &target,
                    i64::try_from(request.candidate.len()).unwrap_or(i64::MAX),
                    request.now,
                )?;
                return Ok(PromoteOutcome::AlreadyCurrent { digest: target });
            }
            Some(_) => {
                return Err(ArtifactError::BaselineVersionConflict {
                    name: crate::contract::policy::baseline_pointer::version_file_name(&target)?,
                });
            }
            // Pointer ahead of version (crash mid-repair): continue without fencing.
            None => skip_fencing = true,
        }
    }

    // 3. Expect fencing (zero writes on mismatch) — skipped when repairing.
    if !skip_fencing {
        match &request.fence {
            PromoteFence::ExpectNoCurrent => {
                if read_current_pointer(root)?.is_some() {
                    return Err(fencing(
                        "pointer already exists; --expect-no-current failed",
                    ));
                }
            }
            PromoteFence::ExpectCurrentDigest(expected) => match read_current_pointer(root)? {
                None => {
                    return Err(fencing("pointer is absent; --expect-current-digest failed"));
                }
                Some(current) if current.digest != *expected => {
                    return Err(fencing(format!(
                        "pointer is {}; expected {expected}",
                        current.digest
                    )));
                }
                Some(_) => {}
            },
        }
    }

    // 4. Admit under the held lock (kind=promotion).
    let op_id = new_op_id();
    let version_name = crate::contract::policy::baseline_pointer::version_file_name(&target)?;
    admit_and_reserve_locked(
        root,
        lock,
        Producer::Promote,
        ReserveRequest {
            op_id: op_id.clone(),
            target: format!("baselines/{version_name} + pointer"),
            payload: target.clone(),
            owner_fenced: None,
            created_at: rfc3339_utc(request.now),
            active_runs: 0,
            protected_count: 0,
            directory_entries: request.directory_entries,
        },
        &NoLeases,
        request.now,
    )?;

    // 5–6. Write version, then switch pointer (hook may observe the window).
    let write_result = (|| -> ArtifactResult<()> {
        write_version_for_digest(root, &target, request.candidate)?;
        if let Some(hook) = between_version_and_pointer {
            hook();
        }
        write_current_pointer(root, lock, &target)?;
        Ok(())
    })();

    match write_result {
        Ok(()) => {
            let settled_delta = i64::try_from(request.candidate.len()).unwrap_or(i64::MAX);
            commit_locked(
                root,
                lock,
                SettleRequest {
                    op_id,
                    settled_delta,
                    final_state: "committed".into(),
                    settled_at: rfc3339_utc(request.now),
                    run_id: None,
                    cap_hash: None,
                },
            )?;
            Ok(PromoteOutcome::Promoted { digest: target })
        }
        Err(err) => {
            let _ = abort_locked(
                root,
                lock,
                SettleRequest {
                    op_id,
                    settled_delta: 0,
                    final_state: "aborted".into(),
                    settled_at: rfc3339_utc(request.now),
                    run_id: None,
                    cap_hash: None,
                },
            );
            Err(err)
        }
    }
}

/// Acquire the maintenance lock and run [`promote_locked`].
pub fn promote(
    root: &RestrictedRoot,
    request: PromoteRequest<'_>,
) -> ArtifactResult<PromoteOutcome> {
    let lock = MaintenanceLock::acquire(root)?;
    promote_locked(root, &lock, request)
}

fn fencing(reason: impl Into<String>) -> ArtifactError {
    ArtifactError::PromotionFencing {
        reason: reason.into(),
        code: PROMOTION_FENCING_EXIT_CODE,
    }
}

/// Commit any open promotion reservation whose payload is `target` (crash after
/// durable pointer switch but before settle).
fn settle_matching_promotion(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    target: &str,
    settled_delta: i64,
    now: SystemTime,
) -> ArtifactResult<()> {
    loop {
        let counter = crate::contract::policy::secure_counter::load_counter(root, lock)?;
        let Some(reservation) = counter.reservations.iter().find(|r| {
            r.kind == crate::contract::policy::secure_counter::ReservationKind::Promotion
                && r.payload == target
        }) else {
            return Ok(());
        };
        commit_locked(
            root,
            lock,
            SettleRequest {
                op_id: reservation.op_id.clone(),
                settled_delta,
                final_state: "committed".into(),
                settled_at: rfc3339_utc(now),
                run_id: None,
                cap_hash: None,
            },
        )?;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod baseline_codec {
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
        let err =
            write_version_for_digest(&root, &d, br#"{"artifact":"v2"}"#).expect_err("conflict");
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
}

#[cfg(all(test, target_os = "linux"))]
mod baseline_promote {
    //! UN-35: baseline promotion CAS under the maintenance lock.

    use std::{
        sync::{Arc, Barrier},
        thread,
        time::{Duration, SystemTime},
    };

    use crate::contract::policy::{
        baseline_pointer::read_current_pointer,
        baseline_promotion::{
            PROMOTION_FENCING_EXIT_CODE, PromoteFence, PromoteOutcome, PromoteRequest,
            content_digest, promote, promote_locked, promote_locked_with_between_hook,
            resolve_promote_fence,
        },
        secure_artifact::{ArtifactError, RestrictedRoot},
        secure_counter::load_counter,
        secure_sweep::{MaintenanceLock, NoReservations, sweep},
    };

    fn root() -> (tempfile::TempDir, RestrictedRoot) {
        let temp = tempfile::tempdir().expect("temp");
        let root = RestrictedRoot::open(temp.path()).expect("open");
        (temp, root)
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000)
    }

    fn req<'a>(candidate: &'a [u8], digest: &'a str, fence: PromoteFence) -> PromoteRequest<'a> {
        PromoteRequest {
            candidate,
            expect_digest: digest,
            fence,
            now: now(),
            directory_entries: 0,
        }
    }

    #[test]
    fn un35_promote_expect_flags_are_exclusive_and_required() {
        assert!(matches!(
            resolve_promote_fence(false, None),
            Err(ArtifactError::PromotionRejected { .. })
        ));
        assert!(matches!(
            resolve_promote_fence(
                true,
                Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            ),
            Err(ArtifactError::PromotionRejected { .. })
        ));
        assert_eq!(
            resolve_promote_fence(true, None).unwrap(),
            PromoteFence::ExpectNoCurrent
        );
    }

    #[test]
    fn un35_promote_integrity_mismatch_writes_nothing() {
        let (_temp, root) = root();
        let candidate = br#"{"ok":true}"#;
        let err = promote(
            &root,
            req(
                candidate,
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                PromoteFence::ExpectNoCurrent,
            ),
        )
        .expect_err("integrity");
        assert!(matches!(err, ArtifactError::PromotionRejected { .. }));
        assert!(read_current_pointer(&root).unwrap().is_none());
    }

    #[test]
    fn un35_promote_first_then_already_current_skips_fencing() {
        let (_temp, root) = root();
        let candidate = br#"{"schema":1,"roles":[]}"#;
        let digest = content_digest(candidate);

        let out = promote(
            &root,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("first");
        assert_eq!(
            out,
            PromoteOutcome::Promoted {
                digest: digest.clone()
            }
        );

        let out = promote(
            &root,
            req(
                candidate,
                &digest,
                PromoteFence::ExpectCurrentDigest(
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .into(),
                ),
            ),
        )
        .expect("already-current");
        assert_eq!(out, PromoteOutcome::AlreadyCurrent { digest });
    }

    #[test]
    fn un35_promote_fencing_mismatch_is_exit_code_three_and_zero_writes() {
        let (_temp, root) = root();
        let first = br#"{"v":1}"#;
        let d1 = content_digest(first);
        promote(&root, req(first, &d1, PromoteFence::ExpectNoCurrent)).expect("seed");

        let second = br#"{"v":2}"#;
        let d2 = content_digest(second);
        let err =
            promote(&root, req(second, &d2, PromoteFence::ExpectNoCurrent)).expect_err("fencing");
        match err {
            ArtifactError::PromotionFencing { code, .. } => {
                assert_eq!(code, PROMOTION_FENCING_EXIT_CODE);
            }
            other => panic!("expected fencing: {other}"),
        }
        assert_eq!(
            read_current_pointer(&root).unwrap().unwrap().digest,
            d1,
            "pointer must remain the winner"
        );
    }

    #[test]
    fn un35_promote_concurrent_fencing_only_one_wins() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));

        let make = |body: &'static [u8]| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let root = RestrictedRoot::open(&path).expect("open");
                let digest = content_digest(body);
                barrier.wait();
                promote(
                    &root,
                    PromoteRequest {
                        candidate: body,
                        expect_digest: &digest,
                        fence: PromoteFence::ExpectNoCurrent,
                        now: now(),
                        directory_entries: 0,
                    },
                )
            })
        };

        let a = make(br#"{"who":"a"}"#);
        let b = make(br#"{"who":"b"}"#);
        let ra = a.join().expect("join a");
        let rb = b.join().expect("join b");

        let wins = [&ra, &rb]
            .iter()
            .filter(|r| matches!(r, Ok(PromoteOutcome::Promoted { .. })))
            .count();
        let fences = [&ra, &rb]
            .iter()
            .filter(
                |r| matches!(r, Err(ArtifactError::PromotionFencing { code, .. }) if *code == 3),
            )
            .count();
        assert_eq!(wins, 1, "exactly one promote must succeed: {ra:?} {rb:?}");
        assert_eq!(fences, 1, "loser must be fencing exit 3: {ra:?} {rb:?}");

        let root = RestrictedRoot::open(&path).expect("open");
        let lock = MaintenanceLock::acquire(&root).expect("lock");
        let counter = load_counter(&root, &lock).expect("counter");
        assert!(counter.reservations.is_empty());
        assert_eq!(counter.settled.len(), 1);
    }

    #[test]
    fn un35_promote_holds_lock_so_concurrent_sweep_cannot_dangle_current() {
        let (_temp, root) = root();
        let candidate = br#"{"keep":true}"#;
        let digest = content_digest(candidate);
        let lock = MaintenanceLock::acquire(&root).expect("lock");

        let root_path = root.display().to_path_buf();
        let outcome = promote_locked_with_between_hook(
            &root,
            &lock,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
            &|| {
                let root2 = RestrictedRoot::open(&root_path).expect("open");
                assert!(
                    MaintenanceLock::try_acquire(&root2).expect("try").is_none(),
                    "sweep/admission must not enter while promote holds the lock"
                );
            },
        )
        .expect("promote");
        assert!(matches!(outcome, PromoteOutcome::Promoted { .. }));

        drop(lock);
        let lock = MaintenanceLock::acquire(&root).expect("lock again");
        let swept = sweep(&root, &lock, &NoReservations, now()).expect("sweep");
        assert!(swept.report_path.is_some());
        assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
    }

    #[test]
    fn un35_promote_settles_reservation_immediately() {
        let (_temp, root) = root();
        let candidate = br#"{"n":1}"#;
        let digest = content_digest(candidate);
        let lock = MaintenanceLock::acquire(&root).expect("lock");
        promote_locked(
            &root,
            &lock,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("promote");
        let counter = load_counter(&root, &lock).expect("load");
        assert!(counter.reservations.is_empty());
        assert_eq!(counter.settled.len(), 1);
        assert_eq!(
            counter.settled[0].kind,
            crate::contract::policy::secure_counter::ReservationKind::Promotion
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod promotion_crash {
    //! UN-40: promotion crash-window end states and retry convergence.

    use std::{
        fs,
        path::Path,
        time::{Duration, SystemTime},
    };

    use crate::contract::policy::{
        baseline_pointer::{
            baselines_dir_fragment, read_current_pointer, write_current_pointer,
            write_version_for_digest,
        },
        baseline_promotion::{
            ALREADY_CURRENT_MARKER, CrashWindow, PromoteFence, PromoteOutcome, PromoteRequest,
            content_digest, promote,
        },
        secure_artifact::{BASELINES_DIR, RestrictedRoot},
        secure_sweep::{MaintenanceLock, NoReservations, TEMP_MIN_AGE, sweep},
    };

    fn root() -> (tempfile::TempDir, RestrictedRoot) {
        let temp = tempfile::tempdir().expect("temp");
        let root = RestrictedRoot::open(temp.path()).expect("open");
        (temp, root)
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000)
    }

    fn req<'a>(candidate: &'a [u8], digest: &'a str, fence: PromoteFence) -> PromoteRequest<'a> {
        PromoteRequest {
            candidate,
            expect_digest: digest,
            fence,
            now: now(),
            directory_entries: 0,
        }
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let file = fs::File::open(path).expect("open for utime");
        file.set_times(fs::FileTimes::new().set_modified(when))
            .expect("set mtime");
    }

    #[test]
    fn un40_crash_windows_are_named() {
        assert_eq!(CrashWindow::W1VersionTemp as u8, 0);
        assert_eq!(CrashWindow::W6FullyCommitted as u8, 5);
    }

    #[test]
    fn un40_w1_version_temp_left_retry_promotes() {
        let (temp, root) = root();
        let baselines = temp.path().join(BASELINES_DIR);
        fs::create_dir_all(&baselines).expect("mkdir");
        let debris = baselines.join(".tmp-w1-version");
        fs::write(&debris, br#"partial"#).expect("tmp");

        let candidate = br#"{"w":1}"#;
        let digest = content_digest(candidate);
        let out = promote(
            &root,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("retry after W1");
        assert!(matches!(out, PromoteOutcome::Promoted { .. }));
        assert!(debris.exists(), "temps are UN-38's job, not promote's");
        assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
    }

    #[test]
    fn un40_w2_version_present_retry_is_idempotent() {
        let (_temp, root) = root();
        let candidate = br#"{"w":2}"#;
        let digest = content_digest(candidate);
        write_version_for_digest(&root, &digest, candidate).expect("pre-plant version");

        let out = promote(
            &root,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("W2 retry");
        assert!(matches!(out, PromoteOutcome::Promoted { .. }));
        assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
    }

    #[test]
    fn un40_w3_version_without_pointer_continues() {
        let (_temp, root) = root();
        let old = br#"{"old":true}"#;
        let old_d = content_digest(old);
        let lock = MaintenanceLock::acquire(&root).expect("lock");
        write_version_for_digest(&root, &old_d, old).expect("old version");
        write_current_pointer(&root, &lock, &old_d).expect("old pointer");
        drop(lock);

        let candidate = br#"{"w":3}"#;
        let digest = content_digest(candidate);
        write_version_for_digest(&root, &digest, candidate).expect("new version only");

        let out = promote(
            &root,
            req(
                candidate,
                &digest,
                PromoteFence::ExpectCurrentDigest(old_d.clone()),
            ),
        )
        .expect("W3 continue");
        assert!(matches!(out, PromoteOutcome::Promoted { .. }));
        assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
    }

    #[test]
    fn un40_w4_pointer_temp_left_retry_promotes() {
        let (temp, root) = root();
        let baselines = temp.path().join(BASELINES_DIR);
        fs::create_dir_all(&baselines).expect("mkdir");
        let debris = baselines.join(".tmp-w4-pointer");
        fs::write(&debris, br#"{"schema_version":1,"digest":"sha256:dead"}"#).expect("tmp");

        let candidate = br#"{"w":4}"#;
        let digest = content_digest(candidate);
        let out = promote(
            &root,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("W4");
        assert!(matches!(out, PromoteOutcome::Promoted { .. }));
        assert!(debris.exists());
    }

    #[test]
    fn un40_w5_pointer_may_already_be_new_or_still_old() {
        let (_temp, root) = root();
        let candidate = br#"{"w":5}"#;
        let digest = content_digest(candidate);

        // New pointer durable (rename done): retry → already-current.
        let lock = MaintenanceLock::acquire(&root).expect("lock");
        write_version_for_digest(&root, &digest, candidate).expect("version");
        write_current_pointer(&root, &lock, &digest).expect("pointer");
        drop(lock);

        let out = promote(
            &root,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("W5 already");
        assert_eq!(
            out,
            PromoteOutcome::AlreadyCurrent {
                digest: digest.clone()
            }
        );
        assert_eq!(out.already_current_marker(), Some(ALREADY_CURRENT_MARKER));
    }

    #[test]
    fn un40_w6_fully_committed_is_already_current() {
        let (_temp, root) = root();
        let candidate = br#"{"w":6}"#;
        let digest = content_digest(candidate);
        promote(
            &root,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("first");

        let out = promote(
            &root,
            req(
                candidate,
                &digest,
                // Wrong fence on purpose — already-current must ignore it.
                PromoteFence::ExpectCurrentDigest(
                    "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                        .into(),
                ),
            ),
        )
        .expect("W6");
        assert_eq!(out, PromoteOutcome::AlreadyCurrent { digest });
        assert_eq!(out.already_current_marker(), Some(ALREADY_CURRENT_MARKER));
    }

    #[test]
    fn un40_already_current_settles_orphan_promotion_reservation() {
        use crate::contract::policy::{
            secure_counter::load_counter,
            secure_hardcap::{new_op_id, rfc3339_utc},
            secure_lifecycle::{NoLeases, ReserveRequest, admit_and_reserve_locked},
            secure_producer::Producer,
        };

        let (_temp, root) = root();
        let candidate = br#"{"orphan":true}"#;
        let digest = content_digest(candidate);
        let lock = MaintenanceLock::acquire(&root).expect("lock");

        // Durable promote without settle (crash between pointer and commit).
        write_version_for_digest(&root, &digest, candidate).expect("version");
        write_current_pointer(&root, &lock, &digest).expect("pointer");
        let version_name =
            crate::contract::policy::baseline_pointer::version_file_name(&digest).unwrap();
        admit_and_reserve_locked(
            &root,
            &lock,
            Producer::Promote,
            ReserveRequest {
                op_id: new_op_id(),
                target: format!("baselines/{version_name} + pointer"),
                payload: digest.clone(),
                owner_fenced: None,
                created_at: rfc3339_utc(now()),
                active_runs: 0,
                protected_count: 0,
                directory_entries: 0,
            },
            &NoLeases,
            now(),
        )
        .expect("orphan reserve");
        assert_eq!(load_counter(&root, &lock).unwrap().reservations.len(), 1);
        drop(lock);

        let out = promote(
            &root,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("settle orphan");
        assert!(matches!(out, PromoteOutcome::AlreadyCurrent { .. }));

        let lock = MaintenanceLock::acquire(&root).expect("lock");
        let counter = load_counter(&root, &lock).expect("load");
        assert!(counter.reservations.is_empty());
        assert_eq!(counter.settled.len(), 1);
    }

    #[test]
    fn un40_tmp_cleanup_delegates_to_sweep_age_threshold() {
        let (temp, root) = root();
        let baselines = temp.path().join(baselines_dir_fragment());
        fs::create_dir_all(&baselines).expect("mkdir");
        let young = baselines.join(".tmp-young");
        let old = baselines.join(".tmp-old");
        fs::write(&young, b"y").expect("young");
        fs::write(&old, b"o").expect("old");
        set_mtime(&young, SystemTime::now() - TEMP_MIN_AGE / 2);
        set_mtime(
            &old,
            SystemTime::now() - TEMP_MIN_AGE - Duration::from_secs(60),
        );

        // Seed a current so sweep has a protected set.
        let candidate = br#"{"keep":true}"#;
        let digest = content_digest(candidate);
        promote(
            &root,
            req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        )
        .expect("seed");

        let lock = MaintenanceLock::acquire(&root).expect("lock");
        let outcome = sweep(&root, &lock, &NoReservations, SystemTime::now()).expect("sweep");
        assert!(outcome.removed_temp_files.contains(&".tmp-old".into()));
        assert!(!outcome.removed_temp_files.contains(&".tmp-young".into()));
        assert!(young.exists());
        assert!(!old.exists());
        assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
    }
}
