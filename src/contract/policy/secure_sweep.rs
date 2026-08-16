//! UN-38: bounding what accumulates under the restricted root.
//!
//! Audit runs and baseline versions pile up, and something has to remove the
//! old ones. That "something" deletes files inside a directory an operator
//! registered for evidence, which makes over-deletion the failure that matters
//! here — losing the baseline a promotion was pinned to, or a run that is still
//! being written, is worse than keeping too much.
//!
//! So deletion is bounded three separate ways, and each one is independent of
//! the others:
//!
//! * a **maintenance lock** held for the whole sweep, shared with promotion and
//!   admission, so a sweep never runs beside something that is creating what it
//!   is about to count;
//! * an **activity test** per run — an unsettled reservation within the last
//!   hour, or a held lease — so a run in progress is skipped even if it is
//!   old enough to look collectable;
//! * a **protected set** for versions — whatever the current pointer names,
//!   plus an explicit manifest — which is exempt and does not count toward the
//!   retention limit, so protecting a version cannot push another one out.
//!
//! Everything the sweep reads or removes goes through the restricted root's
//! descriptor with the same no-follow rules as the writer (UN-32): a sweep that
//! could be redirected by a symlink would be a delete primitive pointed at
//! arbitrary paths.
//!
//! Where this card stops: the reservation counter (UN-57), the sweep report
//! (UN-49), admission and alerting (UN-58), and the pointer's full schema
//! (UN-39) belong to other cards. The seams for them are parameters here, not
//! guesses.

use std::{
    collections::BTreeSet,
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    time::{Duration, SystemTime},
};

use serde::Deserialize;

use crate::contract::policy::secure_artifact::{
    ArtifactError, ArtifactResult, BASELINES_DIR, POINTER_NAME, RUNS_DIR, RestrictedRoot,
    retry_on_interrupt, validate_run_id,
};

/// The one lock. Sweep, promotion and admission all take it, so none of them
/// can observe a half-finished view of what the others are doing.
pub const MAINTENANCE_LOCK: &str = ".maintenance.lock";
/// A run's liveness marker: held by the process that owns the run, so the lock
/// is released by the kernel when that process dies.
pub const LEASE_LOCK: &str = ".lease.lock";
/// The machine-readable protection manifest.
pub const PROTECTED_MANIFEST: &str = "protected.json";

/// How many run batches are kept.
pub const MAX_RUN_BATCHES: usize = 20;
/// How many baseline versions are kept, not counting protected ones.
pub const MAX_BASELINE_VERSIONS: usize = 10;
/// A run younger than this with an unsettled reservation is treated as active.
pub const ACTIVE_RUN_MAX_AGE: Duration = Duration::from_secs(60 * 60);
/// Crash debris is only debris once it has stopped being plausibly in use.
pub const TEMP_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// Whether a run still holds an unsettled reservation.
///
/// The counter itself is UN-57's. This is the seam, not a guess at its shape:
/// the sweep needs one bit per run and nothing else, so asking for exactly that
/// keeps the two cards from having to agree on anything larger.
pub trait ReservationView {
    fn has_unsettled_reservation(&self, run_id: &str) -> bool;
}

/// The view to use before the counter exists.
///
/// Deliberately the *permissive-to-deletion* default rather than the reverse:
/// with no counter, the lease and the age threshold are what protect an active
/// run, and pretending every run holds a reservation would disable the sweep
/// entirely rather than make it safer.
pub struct NoReservations;

impl ReservationView for NoReservations {
    fn has_unsettled_reservation(&self, _run_id: &str) -> bool {
        false
    }
}

/// What a sweep did, in terms an operator can check against the directory.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    pub removed_runs: Vec<String>,
    pub removed_versions: Vec<String>,
    pub removed_temp_files: Vec<String>,
    /// Runs that were old enough to collect but were left alone because
    /// something still holds them. Reported rather than silently skipped: "why
    /// is this still here" is the first question an operator asks of a
    /// retention limit that did not apply.
    pub skipped_active_runs: Vec<String>,
    /// Runs that disappeared between being listed and being removed.
    ///
    /// Kept apart from the active skips on purpose: "somebody else deleted it"
    /// and "something is using it" look identical in a directory listing
    /// afterwards, and only the second is a retention decision.
    pub skipped_vanished_runs: Vec<String>,
    pub protected_versions: Vec<String>,
}

/// The maintenance lock, held for as long as the value lives.
pub struct MaintenanceLock {
    _fd: OwnedFd,
}

impl MaintenanceLock {
    /// Take the lock, blocking until it is free.
    pub fn acquire(root: &RestrictedRoot) -> ArtifactResult<Self> {
        Self::open_and_lock(root, libc::LOCK_EX)
    }

    /// Take the lock only if it is free right now.
    ///
    /// For a caller that would rather do nothing than wait — a sweep is
    /// housekeeping, and blocking a command behind another process's
    /// housekeeping trades a bounded directory for an unbounded wait.
    pub fn try_acquire(root: &RestrictedRoot) -> ArtifactResult<Option<Self>> {
        match Self::open_and_lock(root, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(lock) => Ok(Some(lock)),
            // `EWOULDBLOCK` and `EAGAIN` are the same value on Linux; one arm
            // covers both spellings of "someone else holds it".
            Err(ArtifactError::Io { source, .. })
                if source.raw_os_error() == Some(libc::EWOULDBLOCK) =>
            {
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    fn open_and_lock(root: &RestrictedRoot, how: libc::c_int) -> ArtifactResult<Self> {
        let fd = open_or_create_lock(root.as_raw_fd(), MAINTENANCE_LOCK)?;
        // SAFETY: `fd` is an open descriptor owned by this call.
        if unsafe { libc::flock(fd.as_raw_fd(), how) } < 0 {
            return Err(io_error("flock", MAINTENANCE_LOCK));
        }
        Ok(Self { _fd: fd })
    }
}

/// Run the sweep.
///
/// The caller is responsible for holding [`MaintenanceLock`] — passed in rather
/// than taken here so the lock can span more than the sweep when the caller is
/// also promoting or admitting, which is the whole reason it is one lock.
pub fn sweep(
    root: &RestrictedRoot,
    _lock: &MaintenanceLock,
    reservations: &dyn ReservationView,
    now: SystemTime,
) -> ArtifactResult<SweepOutcome> {
    let mut outcome = SweepOutcome::default();

    // Everything that can refuse is resolved *before* anything is removed. The
    // protected set is read from the pointer and the manifest, and either can
    // be unreadable — discovering that after the runs were already deleted
    // would make "fail-closed" true only of the half that had not happened yet.
    let protected = protected_set(root)?;
    outcome.protected_versions = protected.iter().cloned().collect();

    sweep_runs(root, reservations, now, &mut outcome)?;
    sweep_versions(root, &protected, now, &mut outcome)?;

    Ok(outcome)
}

// ---------------------------------------------------------------- runs

fn sweep_runs(
    root: &RestrictedRoot,
    reservations: &dyn ReservationView,
    now: SystemTime,
    outcome: &mut SweepOutcome,
) -> ArtifactResult<()> {
    let Some(runs) = open_dir_if_present(root.as_raw_fd(), RUNS_DIR)? else {
        return Ok(());
    };

    let mut batches: Vec<Entry> = Vec::new();
    for entry in read_entries(runs.as_raw_fd(), RUNS_DIR)? {
        if entry.name.starts_with(".tmp-") {
            if let Some(claim) = claim_stale_temp(runs.as_raw_fd(), &entry, now)? {
                unlink_at(runs.as_raw_fd(), &entry.name, entry.is_dir)?;
                drop(claim);
                outcome.removed_temp_files.push(entry.name);
            }
            continue;
        }
        // A directory whose name is not a run id was not created by the writer.
        // Left alone on purpose: a sweep that deletes what it does not
        // recognise is a delete primitive with a directory listing for input.
        if validate_run_id(&entry.name).is_err() || !entry.is_dir {
            continue;
        }
        batches.push(entry);
    }

    order_oldest_first(&mut batches);

    let removable = batches.len().saturating_sub(MAX_RUN_BATCHES);
    for entry in batches.into_iter().take(removable) {
        let claim = match claim_for_removal(runs.as_raw_fd(), &entry, reservations, now)? {
            Claim::Held(claim) => claim,
            Claim::Active => {
                outcome.skipped_active_runs.push(entry.name);
                continue;
            }
            Claim::Vanished => {
                outcome.skipped_vanished_runs.push(entry.name);
                continue;
            }
        };
        remove_dir_recursive(runs.as_raw_fd(), &entry.name)?;
        drop(claim);
        outcome.removed_runs.push(entry.name);
    }

    Ok(())
}

/// Two independent reasons a run may still be in use.
///
/// The lease is the reliable one — the kernel releases it when the owning
/// process dies, so a crashed run stops being protected without anyone tidying
/// up. The reservation-plus-age rule covers the window where the run holds
/// state in the counter but has not taken the lease yet; the age bound is what
/// keeps an abandoned reservation from protecting a directory forever.
/// Why a run was not removed, when it was not.
enum Claim {
    Held(RemovalClaim),
    Active,
    Vanished,
}

fn claim_for_removal(
    runs: RawFd,
    entry: &Entry,
    reservations: &dyn ReservationView,
    now: SystemTime,
) -> ArtifactResult<Claim> {
    let dir = match open_dir_if_present(runs, &entry.name)? {
        Some(dir) => dir,
        None => return Ok(Claim::Vanished),
    };

    // Take the lease rather than probe it. Probing and letting go leaves a
    // window in which the run's owner acquires the lease and starts writing
    // while the sweep is midway through deleting the directory — the run would
    // be active and gone at the same time. Holding it for the whole removal
    // means the owner cannot start; if it already has, this fails and the run
    // is skipped.
    let Some(lease) = try_hold_lease(dir.as_raw_fd())? else {
        return Ok(Claim::Active);
    };

    if reservations.has_unsettled_reservation(&entry.name)
        && age_of(entry.modified, now) <= ACTIVE_RUN_MAX_AGE
    {
        return Ok(Claim::Active);
    }

    Ok(Claim::Held(RemovalClaim {
        _lease: lease,
        _dir: dir,
    }))
}

/// A run held still for the duration of its removal.
///
/// The lease descriptor stays open — and so stays locked — until the removal is
/// done. Unlinking the lease file while holding it is fine: the lock lives on
/// the open description, not on the name.
struct RemovalClaim {
    _lease: Option<OwnedFd>,
    _dir: OwnedFd,
}

/// Take the run's lease, or report that its owner has it.
///
/// `Ok(None)` means somebody holds it — the run is live. `Ok(Some(None))` means
/// there is no lease file at all, which is the ordinary case for a finished run.
/// The descriptor is returned rather than released so the caller can hold the
/// run still for as long as it needs.
fn try_hold_lease(run_dir: RawFd) -> ArtifactResult<Option<Option<OwnedFd>>> {
    let Some(fd) = open_existing_lock(run_dir, LEASE_LOCK)? else {
        return Ok(Some(None));
    };
    // SAFETY: `fd` is an open descriptor owned by this call.
    let locked = retry_on_interrupt(|| unsafe {
        libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
    });
    if locked < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            // Same value as `EAGAIN` on Linux: somebody holds the lease.
            Some(libc::EWOULDBLOCK) => Ok(None),
            _ => Err(ArtifactError::Io {
                operation: "flock",
                path: LEASE_LOCK.to_string(),
                source: err,
            }),
        };
    }
    Ok(Some(Some(fd)))
}

// ---------------------------------------------------------------- versions

fn sweep_versions(
    root: &RestrictedRoot,
    protected: &BTreeSet<String>,
    now: SystemTime,
    outcome: &mut SweepOutcome,
) -> ArtifactResult<()> {
    let Some(baselines) = open_dir_if_present(root.as_raw_fd(), BASELINES_DIR)? else {
        return Ok(());
    };

    let mut versions: Vec<Entry> = Vec::new();
    for entry in read_entries(baselines.as_raw_fd(), BASELINES_DIR)? {
        if entry.name.starts_with(".tmp-") {
            if let Some(claim) = claim_stale_temp(baselines.as_raw_fd(), &entry, now)? {
                unlink_at(baselines.as_raw_fd(), &entry.name, entry.is_dir)?;
                drop(claim);
                outcome.removed_temp_files.push(entry.name);
            }
            continue;
        }
        // Only names this module produces are candidates. Treating every file
        // here as a version would make an operator's note in the directory a
        // retention casualty — the same reason an unrecognised run directory is
        // left alone.
        if entry.is_dir || !is_version_file_name(&entry.name) {
            continue;
        }
        // Protected versions are exempt *and* excluded from the count, so
        // protecting one cannot push another out of the retention window.
        if protected.contains(&entry.name) {
            continue;
        }
        versions.push(entry);
    }

    order_oldest_first(&mut versions);

    let removable = versions.len().saturating_sub(MAX_BASELINE_VERSIONS);
    for entry in versions.into_iter().take(removable) {
        unlink_at(baselines.as_raw_fd(), &entry.name, false)?;
        outcome.removed_versions.push(entry.name);
    }

    Ok(())
}

/// What the current pointer names.
///
/// Only the one field, and unknown fields are *accepted* on purpose: the
/// pointer's full schema belongs to UN-39 and will grow. What has to be strict
/// is the property the sweep depends on — if the current version cannot be
/// identified, refuse — and that is enforced by the field being required and by
/// [`version_file_name`], not by rejecting fields this card has not met yet.
#[derive(Debug, Deserialize)]
struct PointerDigest {
    digest: String,
}

/// The manifest's frozen shape. Unknown fields are refused, so a manifest
/// written against a later schema is a reason to stop rather than a set of
/// protections silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectedManifest {
    schema_version: u32,
    protected: Vec<String>,
}

/// Everything the sweep may not delete.
///
/// Fail-closed throughout: an unreadable pointer or an unparseable manifest
/// stops the sweep, because the alternative is deleting while not knowing what
/// was protected. "Absent" is different from "unreadable" — a root with no
/// manifest at all simply has nothing but the current version protected.
fn protected_set(root: &RestrictedRoot) -> ArtifactResult<BTreeSet<String>> {
    let mut protected = BTreeSet::new();

    if let Some(pointer) = crate::contract::policy::secure_artifact::read_pointer(root)? {
        let parsed: PointerDigest =
            serde_json::from_slice(&pointer).map_err(|source| ArtifactError::Io {
                operation: "parse current pointer",
                path: POINTER_NAME.to_string(),
                source: io::Error::new(io::ErrorKind::InvalidData, source),
            })?;
        protected.insert(version_file_name(&parsed.digest)?);
    }

    let manifest_bytes = match open_dir_if_present(root.as_raw_fd(), BASELINES_DIR)? {
        Some(baselines) => read_file_if_present(baselines.as_raw_fd(), PROTECTED_MANIFEST)?,
        None => None,
    };
    if let Some(bytes) = manifest_bytes {
        let manifest: ProtectedManifest =
            serde_json::from_slice(&bytes).map_err(|source| ArtifactError::Io {
                operation: "parse protection manifest",
                path: PROTECTED_MANIFEST.to_string(),
                source: io::Error::new(io::ErrorKind::InvalidData, source),
            })?;
        if manifest.schema_version != 1 {
            return Err(ArtifactError::Io {
                operation: "parse protection manifest",
                path: PROTECTED_MANIFEST.to_string(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "schema_version {} is not supported; refusing to sweep rather than \
                         guess which versions this manifest protects",
                        manifest.schema_version
                    ),
                ),
            });
        }
        for digest in &manifest.protected {
            protected.insert(version_file_name(digest)?);
        }
    }

    Ok(protected)
}

/// Whether a name is one this module writes version files under.
///
/// Canonical form only: 64 lower-case hex characters and the `.json` suffix.
fn is_version_file_name(name: &str) -> bool {
    let Some(hex) = name.strip_suffix(".json") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `sha256:<64 hex>` → `<64 hex>.json`.
///
/// Strict on the way in: a digest that is not exactly this shape means the
/// manifest is not the thing this code was written against, and a mis-parsed
/// protection entry is indistinguishable from no protection at all.
fn version_file_name(digest: &str) -> ArtifactResult<String> {
    let invalid = |reason: &str| ArtifactError::Io {
        operation: "read protection entry",
        path: digest.to_string(),
        source: io::Error::new(io::ErrorKind::InvalidData, reason.to_string()),
    };

    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| invalid("expected a `sha256:` prefix"))?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("expected 64 hex characters after `sha256:`"));
    }
    // Lower case only, and refused rather than normalised. Version files are
    // named by `hex::encode`, which is lower case; accepting an upper-case
    // spelling would build a name that matches no file, and a protection entry
    // that matches nothing is indistinguishable from no protection at all.
    if hex.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(invalid(
            "expected lower-case hex: version files are named in the canonical form, and an \
             entry that matches no file protects nothing",
        ));
    }
    Ok(format!("{hex}.json"))
}

// ---------------------------------------------------------------- entries

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    is_dir: bool,
    modified: SystemTime,
}

/// Oldest first, with a deterministic tie-break.
///
/// mtime is the frozen source of truth for "oldest". Names break ties because
/// two files written in the same timestamp granularity must still be ordered
/// the same way on every run — otherwise which one survives depends on
/// directory order, which is not a decision anybody made.
fn order_oldest_first(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn age_of(modified: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(modified).unwrap_or(Duration::ZERO)
}

/// Crash debris, and only once it has stopped being plausibly in use.
///
/// A `.tmp-` file younger than the threshold may belong to a writer that is
/// mid-sequence right now; the age bound is what separates "in flight" from
/// "left behind", since the writer that made it is gone and cannot say.
fn claim_stale_temp(
    dir: RawFd,
    entry: &Entry,
    now: SystemTime,
) -> ArtifactResult<Option<Option<OwnedFd>>> {
    if age_of(entry.modified, now) <= TEMP_MIN_AGE {
        return Ok(None);
    }

    // Age alone is not "the owner is defunct". Where the temp sits at a level
    // that has a lease, the lease answers that exactly — and the lease is held
    // across the unlink rather than probed and dropped, for the same reason as
    // a run removal: releasing it first leaves a window for the owner to come
    // back between the check and the deletion.
    //
    // Residual, recorded rather than papered over: at a level with no lease and
    // no other owner marker, age is the only signal there is. That is what the
    // threshold is for.
    try_hold_lease(dir)
}

fn read_entries(dir: RawFd, label: &str) -> ArtifactResult<Vec<Entry>> {
    // `fdopendir` takes ownership of the descriptor it is given, so it gets a
    // duplicate: the caller's fd stays valid for the rest of the sweep.
    // SAFETY: `dir` is an open directory descriptor.
    let duplicate = unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io_error("fcntl", label));
    }
    // SAFETY: `duplicate` is a fresh descriptor; `fdopendir` takes it over.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so the descriptor is still ours to close.
        unsafe { libc::close(duplicate) };
        return Err(ArtifactError::Io {
            operation: "fdopendir",
            path: label.to_string(),
            source: err,
        });
    }

    let mut entries = Vec::new();
    loop {
        // `readdir` reports both "end of directory" and "error" as NULL, so
        // errno has to be cleared first: without this, a failed read looks
        // exactly like a complete listing, and the sweep would quietly act on a
        // truncated view of the directory.
        // SAFETY: `__errno_location` returns a valid pointer for this thread.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: `stream` is an open directory stream.
        let raw = unsafe { libc::readdir(stream) };
        if raw.is_null() {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(0) | None => break,
                Some(libc::EINTR) => continue,
                _ => {
                    // SAFETY: `stream` is open and owned here.
                    unsafe { libc::closedir(stream) };
                    return Err(ArtifactError::Io {
                        operation: "readdir",
                        path: label.to_string(),
                        source: err,
                    });
                }
            }
        }
        // SAFETY: `readdir` returned a valid entry pointer.
        let name = unsafe { std::ffi::CStr::from_ptr((*raw).d_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if name == "." || name == ".." {
            continue;
        }

        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let c_name = CString::new(name.as_str()).map_err(|_| ArtifactError::PathEscape {
            component: name.clone(),
        })?;
        // SAFETY: `dir` is open; `c_name` outlives the call; `stat` is a valid
        // out-parameter. `AT_SYMLINK_NOFOLLOW` keeps a planted symlink from
        // being described by whatever it points at.
        let statted =
            unsafe { libc::fstatat(dir, c_name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
        if statted < 0 {
            // Vanished between listing and stat: nothing to account for.
            continue;
        }

        // Symlinks are never swept. They are not artifacts this module wrote,
        // and following one to decide an age would be taking the target's word
        // for the link's.
        if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            continue;
        }

        entries.push(Entry {
            name,
            is_dir: stat.st_mode & libc::S_IFMT == libc::S_IFDIR,
            modified: modified_from(&stat),
        });
    }

    // SAFETY: `stream` is open and owned here; this also closes the duplicate.
    unsafe { libc::closedir(stream) };
    Ok(entries)
}

/// A filesystem object's identity: the pair that stays put across renames.
type Identity = (libc::dev_t, libc::ino_t);

fn identity_of(fd: RawFd, label: &str) -> ArtifactResult<Identity> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open; `stat` is a valid out-parameter.
    if retry_on_interrupt(|| unsafe { libc::fstat(fd, &mut stat) }) < 0 {
        return Err(io_error("fstat", label));
    }
    Ok((stat.st_dev, stat.st_ino))
}

fn identity_at(parent: RawFd, name: &str) -> ArtifactResult<Option<Identity>> {
    let c_name = CString::new(name).map_err(|_| ArtifactError::PathEscape {
        component: name.to_string(),
    })?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `parent` is an open directory fd; `c_name` outlives the call.
    let statted = retry_on_interrupt(|| unsafe {
        libc::fstatat(
            parent,
            c_name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    });
    if statted < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            _ => Err(ArtifactError::Io {
                operation: "fstatat",
                path: name.to_string(),
                source: err,
            }),
        };
    }
    Ok(Some((stat.st_dev, stat.st_ino)))
}

fn modified_from(stat: &libc::stat) -> SystemTime {
    let secs = stat.st_mtime;
    let nanos = stat.st_mtime_nsec as u32;
    if secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::new(secs as u64, nanos)
    } else {
        SystemTime::UNIX_EPOCH - Duration::new(secs.unsigned_abs(), 0)
    }
}

// ---------------------------------------------------------------- fs helpers

fn io_error(operation: &'static str, path: &str) -> ArtifactError {
    ArtifactError::Io {
        operation,
        path: path.to_string(),
        source: io::Error::last_os_error(),
    }
}

fn open_dir_if_present(dir: RawFd, name: &str) -> ArtifactResult<Option<OwnedFd>> {
    let c_name = CString::new(name).map_err(|_| ArtifactError::PathEscape {
        component: name.to_string(),
    })?;
    // SAFETY: `dir` is an open directory fd; `c_name` outlives the call.
    let fd = unsafe {
        libc::openat(
            dir,
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            _ => Err(ArtifactError::Io {
                operation: "openat",
                path: name.to_string(),
                source: err,
            }),
        };
    }
    // SAFETY: a fresh descriptor this call owns.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn open_or_create_lock(dir: RawFd, name: &str) -> ArtifactResult<OwnedFd> {
    let c_name = CString::new(name).map_err(|_| ArtifactError::PathEscape {
        component: name.to_string(),
    })?;
    // SAFETY: `dir` is an open directory fd; `c_name` outlives the call.
    let fd = unsafe {
        libc::openat(
            dir,
            c_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io_error("openat", name));
    }
    // SAFETY: a fresh descriptor this call owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_existing_lock(dir: RawFd, name: &str) -> ArtifactResult<Option<OwnedFd>> {
    let c_name = CString::new(name).map_err(|_| ArtifactError::PathEscape {
        component: name.to_string(),
    })?;
    // SAFETY: `dir` is an open directory fd; `c_name` outlives the call.
    let fd = unsafe {
        libc::openat(
            dir,
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            _ => Err(ArtifactError::Io {
                operation: "openat",
                path: name.to_string(),
                source: err,
            }),
        };
    }
    // SAFETY: a fresh descriptor this call owns.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn read_file_if_present(dir: RawFd, name: &str) -> ArtifactResult<Option<Vec<u8>>> {
    let Some(fd) = open_existing_lock(dir, name)? else {
        return Ok(None);
    };
    let mut contents = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        // SAFETY: `fd` is open for reading; the buffer is valid for its length.
        let read = unsafe {
            libc::read(
                fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len() as libc::size_t,
            )
        };
        if read < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ArtifactError::Io {
                operation: "read",
                path: name.to_string(),
                source: err,
            });
        }
        if read == 0 {
            break;
        }
        contents.extend_from_slice(&buffer[..read as usize]);
    }
    Ok(Some(contents))
}

fn unlink_at(dir: RawFd, name: &str, is_dir: bool) -> ArtifactResult<()> {
    if is_dir {
        return remove_dir_recursive(dir, name);
    }
    let c_name = CString::new(name).map_err(|_| ArtifactError::PathEscape {
        component: name.to_string(),
    })?;
    // SAFETY: `dir` is an open directory fd; `c_name` outlives the call.
    if unsafe { libc::unlinkat(dir, c_name.as_ptr(), 0) } < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(ArtifactError::Io {
            operation: "unlinkat",
            path: name.to_string(),
            source: err,
        });
    }
    Ok(())
}

/// Remove a directory and its contents, descending by descriptor.
///
/// Never by path: resolving `runs/<id>/...` afresh for each removal would give
/// anything that can rename a directory a way to point the removal somewhere
/// else halfway through.
fn remove_dir_recursive(parent: RawFd, name: &str) -> ArtifactResult<()> {
    let Some(dir) = open_dir_if_present(parent, name)? else {
        return Ok(());
    };

    // Remember which directory this actually is. The final `unlinkat` names the
    // entry, not the inode, so a rename slipped in after the contents were
    // removed would have it delete whatever is at that name *now* — the emptied
    // directory would survive and an unrelated one would be gone.
    let opened = identity_of(dir.as_raw_fd(), name)?;

    for entry in read_entries(dir.as_raw_fd(), name)? {
        if entry.is_dir {
            // Never descend across a device boundary. A mount point under a run
            // directory is somebody else's filesystem, and recursing into it
            // would turn a bounded cleanup into a walk of whatever is mounted
            // there. Leaving it means the `AT_REMOVEDIR` below fails with
            // ENOTEMPTY, which is the right outcome: refuse, do not improvise.
            if let Some((device, _)) = identity_at(dir.as_raw_fd(), &entry.name)?
                && device != opened.0
            {
                continue;
            }
        }
        unlink_at(dir.as_raw_fd(), &entry.name, entry.is_dir)?;
    }
    drop(dir);

    if identity_at(parent, name)? != Some(opened) {
        // The name no longer refers to what was emptied. Leaving it is the
        // conservative half of the race: an empty directory is debris the next
        // sweep collects, whereas removing the wrong one cannot be undone.
        return Ok(());
    }

    let c_name = CString::new(name).map_err(|_| ArtifactError::PathEscape {
        component: name.to_string(),
    })?;
    // SAFETY: `parent` is an open directory fd; `c_name` outlives the call.
    if unsafe { libc::unlinkat(parent, c_name.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(ArtifactError::Io {
            operation: "unlinkat",
            path: name.to_string(),
            source: err,
        });
    }
    Ok(())
}

/// Sanity: the sweep's constants are the ones the card froze.
const _: () = {
    assert!(MAX_RUN_BATCHES == 20);
    assert!(MAX_BASELINE_VERSIONS == 10);
};
