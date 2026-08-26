//! UN-51: the capacity counter ledger under a restricted root.
//!
//! The counter is the only place that says how much of the root is spoken for.
//! Reservation lifecycle (UN-57), admission (UN-58), and per-type write ceilings
//! (UN-59) all consume this module; this card freezes schema, strict parse,
//! atomic persistence under the maintenance lock, and a bounded reconcile that
//! a sweep can call without inventing numbers.

use std::{
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::contract::policy::{
    secure_artifact::{
        ArtifactError, ArtifactResult, BASELINES_DIR, POINTER_NAME, RUNS_DIR, RestrictedRoot,
        SWEEP_REPORTS_DIR, clear_errno, read_root_file, validate_run_id, write_root_file_atomic,
    },
    secure_sweep::{MaintenanceLock, PROTECTED_MANIFEST},
};

/// On-disk name of the capacity ledger.
pub const COUNTERS_FILE: &str = ".counters.json";

/// Create-class reservations in flight (delete/unprotect does not consume).
pub const MAX_CREATE_RESERVATIONS: usize = 64;
/// Create-class settlement tombs.
pub const MAX_SETTLED: usize = 64;
/// Delete-settlement tombs (independent of [`MAX_SETTLED`]).
pub const MAX_DELETE_SETTLED: usize = 16;
/// Serialized ledger ceiling (includes emergency headroom).
pub const MAX_COUNTERS_BYTES: usize = 48 * 1024;
/// Bytes of [`MAX_COUNTERS_BYTES`] reserved for delete / recovery writes.
pub const COUNTERS_DELETE_HEADROOM: usize = 8 * 1024;
/// Create admission may not push the ledger past this.
pub const MAX_COUNTERS_BYTES_FOR_CREATE: usize = MAX_COUNTERS_BYTES - COUNTERS_DELETE_HEADROOM;
/// Directory entries examined per listing during reconcile (plan D).
pub const MAX_RECONCILE_DIR_ENTRIES: usize = 1000;
/// Max nesting while summing a single run directory.
pub const MAX_RECONCILE_DEPTH: u32 = 8;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReservationKind {
    Run,
    Promotion,
    Protect,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReservationAction {
    Create,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReservationRecord {
    pub op_id: String,
    pub kind: ReservationKind,
    pub action: ReservationAction,
    pub created_at: String,
    pub max_bytes: u64,
    pub target: String,
    pub payload: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_fenced: Option<bool>,
    pub settle_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettledRecord {
    pub op_id: String,
    pub kind: ReservationKind,
    pub action: ReservationAction,
    pub settled_delta: i64,
    pub final_state: String,
    pub settled_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_fenced: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteSettledRecord {
    pub op_id: String,
    pub kind: ReservationKind,
    pub action: ReservationAction,
    pub settled_delta: i64,
    pub final_state: String,
    pub settled_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityCounter {
    pub schema_version: u32,
    pub runs: u64,
    pub versions: u64,
    pub reports: u64,
    pub total_bytes: u64,
    pub reserved_bytes: u64,
    pub reservations: Vec<ReservationRecord>,
    pub settled: Vec<SettledRecord>,
    pub delete_settled: Vec<DeleteSettledRecord>,
}

impl Default for CapacityCounter {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            runs: 0,
            versions: 0,
            reports: 0,
            total_bytes: 0,
            reserved_bytes: 0,
            reservations: Vec::new(),
            settled: Vec::new(),
            delete_settled: Vec::new(),
        }
    }
}

impl CapacityCounter {
    /// Canonical compact JSON bytes, failing closed on the ledger ceiling.
    ///
    /// `for_create` uses the tighter create-admission ceiling so delete emergency
    /// headroom stays available.
    pub fn to_canonical_bytes(&self, for_create: bool) -> ArtifactResult<Vec<u8>> {
        self.validate_bounds()?;
        self.validate_owner_fenced_shape()?;
        self.validate_delete_settled_shape()?;
        let bytes = serde_json::to_vec(self).map_err(|err| ArtifactError::Io {
            operation: "serde_json::to_vec",
            path: COUNTERS_FILE.to_string(),
            source: io::Error::other(err),
        })?;
        let limit = if for_create {
            MAX_COUNTERS_BYTES_FOR_CREATE
        } else {
            MAX_COUNTERS_BYTES
        };
        if bytes.len() > limit {
            return Err(ArtifactError::CounterTooLarge {
                bytes: bytes.len(),
                limit,
            });
        }
        Ok(bytes)
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> ArtifactResult<Self> {
        let value: Self =
            serde_json::from_slice(bytes).map_err(|err| ArtifactError::CounterInvalid {
                reason: format!("strict parse failed: {err}"),
            })?;
        if value.schema_version != SCHEMA_VERSION {
            return Err(ArtifactError::CounterInvalid {
                reason: format!(
                    "unsupported schema_version {}; only {SCHEMA_VERSION} is accepted",
                    value.schema_version
                ),
            });
        }
        value.validate_bounds()?;
        value.validate_owner_fenced_shape()?;
        value.validate_delete_settled_shape()?;
        Ok(value)
    }

    fn validate_bounds(&self) -> ArtifactResult<()> {
        let create_count = self
            .reservations
            .iter()
            .filter(|r| r.action == ReservationAction::Create)
            .count();
        if create_count > MAX_CREATE_RESERVATIONS {
            return Err(ArtifactError::CounterInvalid {
                reason: format!(
                    "create reservations {create_count} exceed limit {MAX_CREATE_RESERVATIONS}"
                ),
            });
        }
        if self.settled.len() > MAX_SETTLED {
            return Err(ArtifactError::CounterInvalid {
                reason: format!("settled {} exceed limit {MAX_SETTLED}", self.settled.len()),
            });
        }
        if self.delete_settled.len() > MAX_DELETE_SETTLED {
            return Err(ArtifactError::CounterInvalid {
                reason: format!(
                    "delete_settled {} exceed limit {MAX_DELETE_SETTLED}",
                    self.delete_settled.len()
                ),
            });
        }
        Ok(())
    }

    fn validate_delete_settled_shape(&self) -> ArtifactResult<()> {
        for d in &self.delete_settled {
            if d.kind != ReservationKind::Protect || d.action != ReservationAction::Delete {
                return Err(ArtifactError::CounterInvalid {
                    reason: "delete_settled entries require kind=protect and action=delete".into(),
                });
            }
        }
        Ok(())
    }

    fn validate_owner_fenced_shape(&self) -> ArtifactResult<()> {
        for r in &self.reservations {
            if r.kind == ReservationKind::Run {
                if r.owner_fenced.is_none() {
                    return Err(ArtifactError::CounterInvalid {
                        reason: "run reservation requires owner_fenced".into(),
                    });
                }
            } else if r.owner_fenced.is_some() {
                return Err(ArtifactError::CounterInvalid {
                    reason: "owner_fenced is only valid on kind=run".into(),
                });
            }
        }
        for s in &self.settled {
            match (s.kind, s.owner_fenced) {
                (ReservationKind::Run, Some(true)) => {
                    if s.run_id.as_ref().is_none_or(|id| id.is_empty()) {
                        return Err(ArtifactError::CounterInvalid {
                            reason: "owner_fenced run settled requires run_id".into(),
                        });
                    }
                    let Some(hash) = s.cap_hash.as_deref() else {
                        return Err(ArtifactError::CounterInvalid {
                            reason: "owner_fenced run settled requires cap_hash".into(),
                        });
                    };
                    validate_cap_hash(hash)?;
                }
                (ReservationKind::Run, Some(false)) => {
                    if s.run_id.is_some() {
                        return Err(ArtifactError::CounterInvalid {
                            reason: "audit run settled must not carry run_id".into(),
                        });
                    }
                    if s.cap_hash.is_some() {
                        return Err(ArtifactError::CounterInvalid {
                            reason: "audit run settled must not carry cap_hash".into(),
                        });
                    }
                }
                (ReservationKind::Run, None) => {
                    return Err(ArtifactError::CounterInvalid {
                        reason: "run settled requires owner_fenced".into(),
                    });
                }
                (_, Some(_)) => {
                    return Err(ArtifactError::CounterInvalid {
                        reason: "owner_fenced is only valid on kind=run".into(),
                    });
                }
                (_, None) => {
                    if s.run_id.is_some() || s.cap_hash.is_some() {
                        return Err(ArtifactError::CounterInvalid {
                            reason: "run_id/cap_hash are only valid on owner-fenced run settled"
                                .into(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Evict the oldest `delete_settled` entry when at capacity so a new one
    /// can be appended. Deterministic: `settled_at` ascending, then `op_id`.
    pub fn push_delete_settled(&mut self, record: DeleteSettledRecord) {
        if self.delete_settled.len() >= MAX_DELETE_SETTLED {
            self.delete_settled.sort_by(|a, b| {
                a.settled_at
                    .cmp(&b.settled_at)
                    .then_with(|| a.op_id.cmp(&b.op_id))
            });
            self.delete_settled.remove(0);
        }
        self.delete_settled.push(record);
    }

    /// Count only create-class reservations (delete actions do not take a slot).
    pub fn create_reservation_count(&self) -> usize {
        self.reservations
            .iter()
            .filter(|r| r.action == ReservationAction::Create)
            .count()
    }
}

pub(crate) fn validate_cap_hash(value: &str) -> ArtifactResult<()> {
    let rest = value
        .strip_prefix("sha256:")
        .ok_or_else(|| ArtifactError::CounterInvalid {
            reason: "cap_hash must be sha256:<64hex>".into(),
        })?;
    if rest.len() != 64
        || !rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ArtifactError::CounterInvalid {
            reason: "cap_hash must be sha256:<64 lowercase hex>".into(),
        });
    }
    Ok(())
}

/// Load the ledger, or an empty one if the file is absent.
///
/// Caller must hold [`MaintenanceLock`].
pub fn load_counter(
    root: &RestrictedRoot,
    _lock: &MaintenanceLock,
) -> ArtifactResult<CapacityCounter> {
    match read_root_file(root, COUNTERS_FILE)? {
        None => Ok(CapacityCounter::default()),
        Some(bytes) => CapacityCounter::from_canonical_bytes(&bytes),
    }
}

/// Persist the ledger under the maintenance lock.
///
/// `for_create` selects the create-admission byte ceiling; delete / recovery
/// paths pass `false` so they can use the emergency headroom.
pub fn store_counter(
    root: &RestrictedRoot,
    _lock: &MaintenanceLock,
    counter: &CapacityCounter,
    for_create: bool,
) -> ArtifactResult<()> {
    let bytes = counter.to_canonical_bytes(for_create)?;
    // Defence: the plaintext run_cap must never appear in the ledger bytes.
    if contains_run_cap_plaintext(&bytes) {
        return Err(ArtifactError::CounterInvalid {
            reason: "ledger bytes contain run_cap plaintext".into(),
        });
    }
    write_root_file_atomic(root, COUNTERS_FILE, &bytes)
}

fn contains_run_cap_plaintext(bytes: &[u8]) -> bool {
    bytes.windows(8).any(|w| w == b"run_cap=")
}

/// Rebuild the on-disk counts from what is actually under the root.
///
/// Reservations / settled tombs are left alone — those are UN-57's. This only
/// aligns `runs` / `versions` / `reports` / `total_bytes` with a bounded walk
/// of known directories so a sweep can correct drift without inventing policy.
///
/// Walks by descriptor (`openat` + `O_NOFOLLOW` + `AT_SYMLINK_NOFOLLOW`), never
/// by resolved path, and caps entries per listing / nesting depth so a planted
/// tree cannot turn reconcile into an unbounded crawl.
pub fn reconcile_counts_from_disk(
    root: &RestrictedRoot,
    _lock: &MaintenanceLock,
    counter: &mut CapacityCounter,
) -> ArtifactResult<()> {
    let mut runs = 0u64;
    let mut versions = 0u64;
    let mut reports = 0u64;
    let mut total_bytes = 0u64;

    count_runs(root, &mut runs, &mut total_bytes)?;
    count_versions(root, &mut versions, &mut total_bytes)?;
    count_reports(root, &mut reports, &mut total_bytes)?;

    counter.runs = runs;
    counter.versions = versions;
    counter.reports = reports;
    counter.total_bytes = total_bytes;
    Ok(())
}

fn open_known_dir(root: &RestrictedRoot, relative: &str) -> ArtifactResult<Option<OwnedFd>> {
    match root.open_dir(Path::new(relative), false) {
        Ok(fd) => Ok(Some(fd)),
        Err(ArtifactError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn count_runs(root: &RestrictedRoot, runs: &mut u64, total_bytes: &mut u64) -> ArtifactResult<()> {
    let Some(dir) = open_known_dir(root, RUNS_DIR)? else {
        return Ok(());
    };
    for entry in list_entries(dir.as_raw_fd(), RUNS_DIR)? {
        if entry.name.starts_with('.') || !entry.is_dir {
            continue;
        }
        if validate_run_id(&entry.name).is_err() {
            continue;
        }
        let Some(child) = open_child_dir(dir.as_raw_fd(), &entry.name)? else {
            continue;
        };
        *runs = runs.saturating_add(1);
        *total_bytes = total_bytes.saturating_add(dir_size_fd(child.as_raw_fd(), 0)?);
    }
    Ok(())
}

fn count_versions(
    root: &RestrictedRoot,
    versions: &mut u64,
    total_bytes: &mut u64,
) -> ArtifactResult<()> {
    let Some(dir) = open_known_dir(root, BASELINES_DIR)? else {
        return Ok(());
    };
    for entry in list_entries(dir.as_raw_fd(), BASELINES_DIR)? {
        let name = &entry.name;
        if name.starts_with('.') || name == POINTER_NAME || name == PROTECTED_MANIFEST {
            continue;
        }
        if entry.is_dir || entry.is_symlink {
            continue;
        }
        // 64 hex + ".json"
        if name.len() == 69
            && name.ends_with(".json")
            && name.as_bytes()[..64]
                .iter()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            *versions = versions.saturating_add(1);
            *total_bytes = total_bytes.saturating_add(entry.size);
        }
    }
    Ok(())
}

fn count_reports(
    root: &RestrictedRoot,
    reports: &mut u64,
    total_bytes: &mut u64,
) -> ArtifactResult<()> {
    let Some(dir) = open_known_dir(root, SWEEP_REPORTS_DIR)? else {
        return Ok(());
    };
    for entry in list_entries(dir.as_raw_fd(), SWEEP_REPORTS_DIR)? {
        if entry.is_dir || entry.is_symlink {
            continue;
        }
        let Some(stem) = entry.name.strip_suffix(".json") else {
            continue;
        };
        if validate_run_id(stem).is_ok() {
            *reports = reports.saturating_add(1);
            *total_bytes = total_bytes.saturating_add(entry.size);
        }
    }
    Ok(())
}

struct ListedEntry {
    name: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
}

fn list_entries(dir: RawFd, label: &str) -> ArtifactResult<Vec<ListedEntry>> {
    // `fdopendir` consumes the fd it is given, so duplicate first.
    // SAFETY: `dir` is an open directory descriptor.
    let duplicate = unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(ArtifactError::Io {
            operation: "fcntl",
            path: label.to_string(),
            source: io::Error::last_os_error(),
        });
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
        if entries.len() >= MAX_RECONCILE_DIR_ENTRIES {
            break;
        }
        // `readdir` reports both "end of directory" and "error" as NULL, so
        // errno has to be cleared first to tell them apart.
        clear_errno();
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
        let c_name = CString::new(name.as_str())
            .map_err(|_| ArtifactError::NotABareName { name: name.clone() })?;
        // SAFETY: `dir` is open; `c_name` outlives the call; `AT_SYMLINK_NOFOLLOW`
        // keeps a planted symlink from being described by its target.
        let statted =
            unsafe { libc::fstatat(dir, c_name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
        if statted < 0 {
            continue;
        }
        let mode = stat.st_mode & libc::S_IFMT;
        entries.push(ListedEntry {
            name,
            is_dir: mode == libc::S_IFDIR,
            is_symlink: mode == libc::S_IFLNK,
            size: if mode == libc::S_IFREG {
                stat.st_size as u64
            } else {
                0
            },
        });
    }

    // SAFETY: `stream` is open and owned here; this also closes the duplicate.
    unsafe { libc::closedir(stream) };
    Ok(entries)
}

fn open_child_dir(parent: RawFd, name: &str) -> ArtifactResult<Option<OwnedFd>> {
    let c_name = CString::new(name).map_err(|_| ArtifactError::NotABareName {
        name: name.to_string(),
    })?;
    // SAFETY: `parent` is an open directory fd; `c_name` outlives the call.
    let fd = unsafe {
        libc::openat(
            parent,
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            // Symlink masquerading as a directory: refuse, do not follow.
            Some(libc::ENOTDIR) | Some(libc::ELOOP) => Ok(None),
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

fn dir_size_fd(dir: RawFd, depth: u32) -> ArtifactResult<u64> {
    if depth >= MAX_RECONCILE_DEPTH {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in list_entries(dir, ".")? {
        if entry.is_symlink {
            continue;
        }
        if entry.is_dir {
            let Some(child) = open_child_dir(dir, &entry.name)? else {
                continue;
            };
            total = total.saturating_add(dir_size_fd(child.as_raw_fd(), depth + 1)?);
        } else {
            total = total.saturating_add(entry.size);
        }
    }
    Ok(total)
}

const _: () = {
    assert!(MAX_CREATE_RESERVATIONS == 64);
    assert!(MAX_SETTLED == 64);
    assert!(MAX_DELETE_SETTLED == 16);
    assert!(MAX_COUNTERS_BYTES == 48 * 1024);
    assert!(COUNTERS_DELETE_HEADROOM == 8 * 1024);
    assert!(MAX_RECONCILE_DIR_ENTRIES == 1000);
    assert!(MAX_RECONCILE_DEPTH == 8);
};
