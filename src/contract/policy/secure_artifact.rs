//! UN-32: writing audit artifacts into a restricted directory, safely.
//!
//! The artifacts an audit produces are not ordinary output. Some of them carry
//! the differences an operator is not allowed to publish; all of them are written by a
//! command that may run as a privileged service account into a directory an
//! operator configured. That combination is what makes the *path* the security
//! boundary rather than the content: if a component of the path can be replaced
//! with a symlink between the check and the write, the artifact lands somewhere
//! nobody chose.
//!
//! So the root is opened once and held as a directory file descriptor, and
//! every subsequent operation is relative to that descriptor with `O_NOFOLLOW`.
//! A path resolved against a live fd cannot be redirected by renaming a
//! directory afterwards, and a component that turns out to be a symlink fails
//! rather than being followed. `..` and absolute components are rejected
//! outright — not normalised, since normalising a path is how an escape gets
//! smuggled past a check.
//!
//! The write sequences are frozen per artifact kind (see the card's matrix) and
//! implemented here rather than left to each caller, because the difference
//! between them is what the crash behaviour of each artifact is:
//!
//! * **run outputs** are written straight to their final path under `O_EXCL`
//!   and fsync'd. There is no rename, so a crash leaves a partial file — which
//!   is correct here: the run is identified as complete by the process exit
//!   status and the `run_id=` line, so a partial file is a failed run's debris
//!   and the sweep collects it.
//! * **baseline version files** are immutable once written, so they go through
//!   a temp file and `RENAME_NOREPLACE`: a second writer must not be able to
//!   replace a version that already exists. Re-writing the same digest is
//!   allowed only after a byte-exact read-back.
//! * **the current pointer** is the one thing that is meant to be replaced, so
//!   it uses a plain atomic rename.
//!
//! Linux only, deliberately: `RENAME_NOREPLACE` has no portable equivalent, and
//! silently degrading to a non-atomic stand-in would make the guarantee a fiction on
//! exactly the platforms where nobody checked.

use std::{
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

/// Where the immutable baseline versions live, relative to the root.
pub const BASELINES_DIR: &str = "baselines";
/// Where each run's outputs live, relative to the root.
pub const RUNS_DIR: &str = "runs";
/// The pointer's name. Derived internally and never taken from a caller: a
/// second path parameter is a second thing to get wrong, and there is only ever
/// one current pointer per root.
pub const POINTER_NAME: &str = "current.json";
/// Sweep audit reports (UN-49). Separate from `runs/` so a retention sweep of
/// run batches cannot collect the report of the sweep that just ran.
pub const SWEEP_REPORTS_DIR: &str = "sweep-reports";

/// Bounded retries when claiming a run directory (card: ≤ 5).
const RUN_ID_CLAIM_ATTEMPTS: usize = 5;

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error(
        "--restricted-root is required: audit artifacts are only ever written beneath a root \
         registered in the deployment table"
    )]
    RestrictedRootMissing,
    #[error(
        "restricted artifact writing requires Linux: it depends on RENAME_NOREPLACE, which has \
         no portable equivalent. Run the audit from a Linux host"
    )]
    UnsupportedPlatform,
    #[error("failed to open the restricted root {path}: {source}")]
    RootOpen { path: PathBuf, source: io::Error },
    #[error(
        "path component `{component}` may not appear in a restricted path: only plain names \
         descending from the root are accepted"
    )]
    PathEscape { component: String },
    #[error("`{name}` must be a bare file name: a directory component is not accepted here")]
    NotABareName { name: String },
    #[error("{path} is not a regular file")]
    NotARegularFile { path: String },
    #[error(
        "{path} has {links} names: a restricted artifact has exactly one, so this entry is an \
         alias for a file that was not created here"
    )]
    AliasedFile { path: String, links: u64 },
    #[error("`{value}` is not a valid run id")]
    InvalidRunId { value: String },
    #[error(
        "`{value}` is not a valid candidate reference: expected `<run-id>/<file-name>`, relative \
         to the restricted root"
    )]
    InvalidCandidateReference { value: String },
    #[error(
        "could not claim a fresh run directory after {attempts} attempts; another process may be \
         creating runs at the same rate, or the clock is not advancing"
    )]
    RunIdExhausted { attempts: usize },
    #[error(
        "baseline version {name} already exists with different content; a version file is \
         immutable once written"
    )]
    BaselineVersionConflict { name: String },
    #[error(
        "sweep report is {bytes} bytes, exceeding the {limit}-byte write ceiling; refusing \
         rather than truncating"
    )]
    ReportTooLarge { bytes: usize, limit: usize },
    #[error(
        "artifact write is {bytes} bytes, exceeding the {limit}-byte hard ceiling for {class}; \
         refusing rather than truncating"
    )]
    WriteTooLarge {
        class: &'static str,
        bytes: usize,
        limit: usize,
    },
    #[error(
        "capacity counter is {bytes} bytes, exceeding the {limit}-byte ledger ceiling; refusing \
         rather than truncating"
    )]
    CounterTooLarge { bytes: usize, limit: usize },
    #[error("capacity counter rejected: {reason}")]
    CounterInvalid { reason: String },
    #[error("capacity admission denied: {reason}")]
    AdmissionDenied { reason: String },
    #[error("reservation lifecycle rejected: {reason}")]
    LifecycleRejected { reason: String },
    #[error("baseline pointer rejected: {reason}")]
    PointerInvalid { reason: String },
    #[error("baseline promotion rejected: {reason}")]
    PromotionRejected { reason: String },
    #[error("baseline promotion fencing failed: {reason} (frozen exit code {code})")]
    PromotionFencing { reason: String, code: i32 },
    #[error("{operation} on {path} failed: {source}")]
    Io {
        operation: &'static str,
        path: String,
        source: io::Error,
    },
}

impl ArtifactError {
    fn io(operation: &'static str, path: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

pub type ArtifactResult<T> = Result<T, ArtifactError>;

/// A restricted root, held open.
///
/// `Debug` prints the path it was opened as, not the descriptor: the number
/// means nothing to a reader, and the path is what identifies the root.
///
/// The descriptor *is* the anchor: every path below is resolved against it, so
/// replacing or renaming the root directory after this point cannot redirect a
/// write. Dropping the value closes it.
pub struct RestrictedRoot {
    fd: OwnedFd,
    display: PathBuf,
}

impl std::fmt::Debug for RestrictedRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestrictedRoot")
            .field("path", &self.display)
            .finish()
    }
}

impl RestrictedRoot {
    /// Open the root named by the caller.
    ///
    /// `O_NOFOLLOW` applies to the final component: a root that is itself a
    /// symlink is refused rather than followed, because "the root" then means
    /// whatever the link points at today.
    ///
    /// Components *above* the root are resolved normally, and that is
    /// deliberate: the root path comes from the deployment registration, it is
    /// resolved once before anything below it exists, and refusing every
    /// symlink on the way to it would reject ordinary layouts where `/var` or a
    /// home directory is a link. The boundary this module defends is everything
    /// *below* the root, which is where names an attacker can create appear.
    pub fn open(path: impl AsRef<Path>) -> ArtifactResult<Self> {
        require_linux()?;
        let path = path.as_ref();
        let c_path = cstring(path)?;
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(ArtifactError::RootOpen {
                path: path.to_path_buf(),
                source: io::Error::last_os_error(),
            });
        }
        Ok(Self {
            // SAFETY: `fd` is a fresh descriptor this call owns.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            display: path.to_path_buf(),
        })
    }

    /// The required-argument rule, expressed where it can be tested.
    ///
    /// The CLI flag itself belongs to the command card; what belongs here is
    /// that there is no way to obtain a writer without naming a root, so no
    /// caller can end up writing to a default location nobody registered.
    pub fn from_option(path: Option<impl AsRef<Path>>) -> ArtifactResult<Self> {
        match path {
            Some(path) => Self::open(path),
            None => Err(ArtifactError::RestrictedRootMissing),
        }
    }

    pub fn display(&self) -> &Path {
        &self.display
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The anchor descriptor, for the sweep's relative operations (UN-38).
    ///
    /// Shared rather than re-derived: a sweep that resolved the root
    /// independently would be a delete primitive with its own idea of where the
    /// root is.
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.raw()
    }

    /// Open a directory relative to the root, creating it if asked.
    ///
    /// Walks one component at a time with `O_NOFOLLOW`, so a symlink anywhere
    /// along the way is an error rather than a redirection.
    pub(crate) fn open_dir(&self, relative: &Path, create: bool) -> ArtifactResult<OwnedFd> {
        let mut current = dup_fd(self.raw(), &self.display)?;
        for component in relative.components() {
            let name = plain_name(&component)?;
            if create {
                let c_name = cstring(Path::new(&name))?;
                // SAFETY: `current` is an open directory fd; `c_name` outlives the call.
                let made = unsafe { libc::mkdirat(current.as_raw_fd(), c_name.as_ptr(), 0o700) };
                if made < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EEXIST) {
                        return Err(ArtifactError::io("mkdirat", name.clone(), err));
                    }
                }
            }
            current = openat_dir(current.as_raw_fd(), &name)?;
        }
        Ok(current)
    }
}

/// Create the run directory for a freshly generated id.
///
/// The id is generated here and never accepted from a caller: a caller-supplied
/// id is a caller-chosen directory, which is the same escape the root fd exists
/// to prevent, and it also lets two runs be told apart only by trust. The
/// `mkdirat` is the claim — it fails if the name is taken, so two processes
/// racing on the same second cannot land in the same directory.
pub struct RunDir {
    run_id: String,
    fd: OwnedFd,
}

impl std::fmt::Debug for RunDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunDir")
            .field("run_id", &self.run_id)
            .finish()
    }
}

impl RunDir {
    pub fn create(root: &RestrictedRoot) -> ArtifactResult<Self> {
        Self::claim(root, generate_run_id)
    }

    /// [`Self::create`] with the id generator supplied — **test only**.
    ///
    /// Exists so the collision path can be exercised for what it is: forcing a
    /// repeated id is the only way to reach the bounded retry, and waiting for a
    /// real clock collision is not a test.
    ///
    /// Deliberately not public. A caller able to supply the generator is a
    /// caller able to choose the run directory, which is the same thing AC-4
    /// refuses — the id being un-suppliable is the property, so the seam that
    /// would undo it does not exist outside tests.
    #[cfg(test)]
    pub(crate) fn create_with(
        root: &RestrictedRoot,
        next_id: impl FnMut() -> String,
    ) -> ArtifactResult<Self> {
        Self::claim(root, next_id)
    }

    fn claim(root: &RestrictedRoot, mut next_id: impl FnMut() -> String) -> ArtifactResult<Self> {
        let runs = root.open_dir(Path::new(RUNS_DIR), true)?;

        for _ in 0..RUN_ID_CLAIM_ATTEMPTS {
            let run_id = next_id();
            validate_run_id(&run_id)?;
            match Self::try_mkdir(runs.as_raw_fd(), &run_id)? {
                Some(fd) => return Ok(Self { run_id, fd }),
                None => continue,
            }
        }

        Err(ArtifactError::RunIdExhausted {
            attempts: RUN_ID_CLAIM_ATTEMPTS,
        })
    }

    /// Claim a predetermined run id (admission already reserved this target).
    pub(crate) fn claim_exact(root: &RestrictedRoot, run_id: &str) -> ArtifactResult<Self> {
        validate_run_id(run_id)?;
        let runs = root.open_dir(Path::new(RUNS_DIR), true)?;
        match Self::try_mkdir(runs.as_raw_fd(), run_id)? {
            Some(fd) => Ok(Self {
                run_id: run_id.to_string(),
                fd,
            }),
            None => Err(ArtifactError::io(
                "mkdirat",
                run_id.to_string(),
                io::Error::from_raw_os_error(libc::EEXIST),
            )),
        }
    }

    fn try_mkdir(runs: RawFd, run_id: &str) -> ArtifactResult<Option<OwnedFd>> {
        let c_name = cstring(Path::new(run_id))?;
        // SAFETY: `runs` is an open directory fd; `c_name` outlives the call.
        let made = unsafe { libc::mkdirat(runs, c_name.as_ptr(), 0o700) };
        if made == 0 {
            return Ok(Some(openat_dir(runs, run_id)?));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(ArtifactError::io("mkdirat", run_id.to_string(), err));
        }
        Ok(None)
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The single machine-readable line a caller prints for this run.
    ///
    /// One line, one format: a runbook that has to parse prose is a runbook
    /// that breaks when the prose is improved.
    pub fn run_id_line(&self) -> String {
        format!("run_id={}", self.run_id)
    }

    /// Write one run output.
    ///
    /// `name` must be a bare file name — accepting a path here would let the
    /// caller choose a directory, which is exactly what the run layout exists to
    /// decide. Final path, `O_EXCL`, `0600`, fsync the file, fsync the
    /// directory, no rename: a crash leaves a partial file at the final path,
    /// and the run is known to have failed by its exit status.
    ///
    /// Size is gated by the inferred write-class hard ceiling (UN-59): oversize
    /// fails closed and does not truncate.
    pub fn write_output(&self, name: &str, contents: &[u8]) -> ArtifactResult<()> {
        let name = bare_name(name)?;
        crate::contract::policy::secure_capacity::hard_cap_violation(
            crate::contract::policy::secure_capacity::infer_write_class(&name),
            contents.len(),
        )
        .map_err(|(bytes, limit)| ArtifactError::WriteTooLarge {
            class: crate::contract::policy::secure_capacity::infer_write_class(&name).name(),
            bytes,
            limit,
        })?;
        let file = openat_create_exclusive(self.fd.as_raw_fd(), &name)?;
        write_all(file.as_raw_fd(), contents, &name)?;
        fsync(file.as_raw_fd(), &name)?;
        fsync(self.fd.as_raw_fd(), RUNS_DIR)?;
        Ok(())
    }

    /// Open an existing run directory (does not create).
    pub(crate) fn open(root: &RestrictedRoot, run_id: &str) -> ArtifactResult<Self> {
        validate_run_id(run_id)?;
        let runs = root.open_dir(Path::new(RUNS_DIR), false)?;
        let fd = openat_dir(runs.as_raw_fd(), run_id)?;
        Ok(Self {
            run_id: run_id.to_string(),
            fd,
        })
    }

    /// Read a bare regular file under this run, or `None` if absent.
    pub(crate) fn read_file(&self, name: &str) -> ArtifactResult<Option<Vec<u8>>> {
        let name = bare_name(name)?;
        read_regular_if_present(self.fd.as_raw_fd(), &name)
    }

    /// Atomically replace a bare file under this run (temp + fsync + rename + dir fsync).
    ///
    /// Used by Kill Switch evidence append (UN-56): crash leaves a stale temp;
    /// the committed file stays the previous bytes.
    pub(crate) fn replace_file(&self, name: &str, contents: &[u8]) -> ArtifactResult<()> {
        let name = bare_name(name)?;
        crate::contract::policy::secure_capacity::hard_cap_violation(
            crate::contract::policy::secure_capacity::infer_write_class(&name),
            contents.len(),
        )
        .map_err(|(bytes, limit)| ArtifactError::WriteTooLarge {
            class: crate::contract::policy::secure_capacity::infer_write_class(&name).name(),
            bytes,
            limit,
        })?;
        let temp = format!(".tmp-{}", random_suffix());
        let file = openat_create_exclusive(self.fd.as_raw_fd(), &temp)?;
        write_all(file.as_raw_fd(), contents, &temp)?;
        fsync(file.as_raw_fd(), &temp)?;
        drop(file);
        if let Err(err) = renameat(self.fd.as_raw_fd(), &temp, &name) {
            unlinkat(self.fd.as_raw_fd(), &temp);
            return Err(ArtifactError::io("renameat", name, err));
        }
        fsync(self.fd.as_raw_fd(), RUNS_DIR)?;
        Ok(())
    }

    /// Open-or-create a lock file under this run and take `flock(LOCK_EX)`.
    ///
    /// The returned fd holds the lock until dropped (kernel unlocks on close).
    pub(crate) fn lock_exclusive(&self, lock_name: &str) -> ArtifactResult<OwnedFd> {
        let name = bare_name(lock_name)?;
        let c_name = cstring(Path::new(&name))?;
        // SAFETY: run dir fd is open; `c_name` outlives the call.
        let fd = retry_on_interrupt(|| unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        });
        if fd < 0 {
            return Err(ArtifactError::io(
                "openat",
                name.clone(),
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: fresh descriptor owned here.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        if retry_on_interrupt(|| unsafe { libc::flock(owned.as_raw_fd(), libc::LOCK_EX) }) < 0 {
            return Err(ArtifactError::io("flock", name, io::Error::last_os_error()));
        }
        Ok(owned)
    }
}

/// Write an immutable baseline version file.
///
/// `RENAME_NOREPLACE` is the point: a version file is named after the digest of
/// what it contains, so a second writer arriving with the same name is either
/// writing the same bytes — in which case there is nothing to do — or the name
/// no longer means what it says. The read-back is what distinguishes the two,
/// and it is a byte comparison rather than a digest recomputation, because
/// recomputing a digest from the same code that wrote it proves nothing.
pub fn write_baseline_version(
    root: &RestrictedRoot,
    name: &str,
    contents: &[u8],
) -> ArtifactResult<()> {
    crate::contract::policy::secure_capacity::hard_cap_violation(
        crate::contract::policy::secure_capacity::WriteClass::CandidateOrBaseline,
        contents.len(),
    )
    .map_err(|(bytes, limit)| ArtifactError::WriteTooLarge {
        class: "candidate/baseline",
        bytes,
        limit,
    })?;
    let name = bare_name(name)?;
    let dir = root.open_dir(Path::new(BASELINES_DIR), true)?;

    if let Some(existing) = read_regular_if_present(dir.as_raw_fd(), &name)? {
        return if existing == contents {
            Ok(())
        } else {
            Err(ArtifactError::BaselineVersionConflict { name })
        };
    }

    let temp = format!(".tmp-{}", random_suffix());
    let file = openat_create_exclusive(dir.as_raw_fd(), &temp)?;
    write_all(file.as_raw_fd(), contents, &temp)?;
    fsync(file.as_raw_fd(), &temp)?;
    drop(file);

    match rename_noreplace(dir.as_raw_fd(), &temp, &name) {
        Ok(()) => {}
        Err(err) => {
            unlinkat(dir.as_raw_fd(), &temp);
            if err.raw_os_error() == Some(libc::EEXIST) {
                // Someone else landed the same name while this was being
                // written. Same rule as above: identical content is fine.
                let existing = read_regular_if_present(dir.as_raw_fd(), &name)?;
                return match existing {
                    Some(existing) if existing == contents => Ok(()),
                    _ => Err(ArtifactError::BaselineVersionConflict { name }),
                };
            }
            return Err(ArtifactError::io("renameat2", name, err));
        }
    }

    fsync(dir.as_raw_fd(), BASELINES_DIR)?;
    Ok(())
}

/// Replace the current pointer.
///
/// The one artifact meant to be replaced, so a plain rename — atomic, and the
/// reader either sees the old pointer or the new one. Serialising concurrent
/// promotions is the promotion card's job; this is the write primitive it uses.
pub(crate) fn replace_pointer(root: &RestrictedRoot, contents: &[u8]) -> ArtifactResult<()> {
    replace_baseline_file(root, POINTER_NAME, contents)
}

/// Atomically replace a bare file under `baselines/` (temp + fsync + rename +
/// directory fsync). Used for the current pointer and `protected.json` (UN-55).
pub(crate) fn replace_baseline_file(
    root: &RestrictedRoot,
    name: &str,
    contents: &[u8],
) -> ArtifactResult<()> {
    let name = bare_name(name)?;
    let dir = root.open_dir(Path::new(BASELINES_DIR), true)?;
    let temp = format!(".tmp-{}", random_suffix());

    let file = openat_create_exclusive(dir.as_raw_fd(), &temp)?;
    write_all(file.as_raw_fd(), contents, &temp)?;
    fsync(file.as_raw_fd(), &temp)?;
    drop(file);

    if let Err(err) = renameat(dir.as_raw_fd(), &temp, &name) {
        unlinkat(dir.as_raw_fd(), &temp);
        return Err(ArtifactError::io("renameat", name, err));
    }
    fsync(dir.as_raw_fd(), BASELINES_DIR)?;
    Ok(())
}

/// Write a file under a root-relative directory with the run-output rules:
/// bare name, `O_EXCL`, `0600`, file fsync, directory fsync. No rename.
///
/// Used by the sweep report (UN-49): a crash leaves a partial file at the
/// final path, and the caller knows the write failed by the returned error.
pub(crate) fn write_exclusive_under(
    root: &RestrictedRoot,
    relative_dir: &str,
    name: &str,
    contents: &[u8],
) -> ArtifactResult<()> {
    let name = bare_name(name)?;
    let dir = root.open_dir(Path::new(relative_dir), true)?;
    let file = openat_create_exclusive(dir.as_raw_fd(), &name)?;
    write_all(file.as_raw_fd(), contents, &name)?;
    fsync(file.as_raw_fd(), &name)?;
    fsync(dir.as_raw_fd(), relative_dir)?;
    Ok(())
}

/// Read a regular file directly under the restricted root, or `None` if absent.
pub(crate) fn read_root_file(root: &RestrictedRoot, name: &str) -> ArtifactResult<Option<Vec<u8>>> {
    let name = bare_name(name)?;
    read_regular_if_present(root.as_raw_fd(), &name)
}

/// Atomically replace a regular file directly under the restricted root.
///
/// Temp file + fsync + plain rename + directory fsync: the same durability
/// sequence as the current-pointer write. Used for the capacity counter
/// (UN-51), which must be replaced under the maintenance lock.
pub(crate) fn write_root_file_atomic(
    root: &RestrictedRoot,
    name: &str,
    contents: &[u8],
) -> ArtifactResult<()> {
    let name = bare_name(name)?;
    let temp = format!(".tmp-{}", random_suffix());
    let file = openat_create_exclusive(root.as_raw_fd(), &temp)?;
    write_all(file.as_raw_fd(), contents, &temp)?;
    fsync(file.as_raw_fd(), &temp)?;
    drop(file);

    if let Err(err) = renameat(root.as_raw_fd(), &temp, &name) {
        unlinkat(root.as_raw_fd(), &temp);
        return Err(ArtifactError::io("renameat", name, err));
    }
    fsync(root.as_raw_fd(), ".")?;
    Ok(())
}

/// Read a bare file under `baselines/`, or `None` if the directory or file is absent.
pub fn read_baseline_file(root: &RestrictedRoot, name: &str) -> ArtifactResult<Option<Vec<u8>>> {
    let name = bare_name(name)?;
    let dir = match root.open_dir(Path::new(BASELINES_DIR), false) {
        Ok(dir) => dir,
        Err(ArtifactError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    read_regular_if_present(dir.as_raw_fd(), &name)
}

/// Read the current pointer, if there is one.
///
/// The path is derived, never passed in: one current pointer per root, and a
/// second path parameter would only be another way to read the wrong file.
pub fn read_pointer(root: &RestrictedRoot) -> ArtifactResult<Option<Vec<u8>>> {
    // Opened without `create`: a read path that makes a directory has written
    // to the root, which is exactly the posture the read rules exist to hold.
    // A missing `baselines/` is the same answer as a missing pointer.
    let dir = match root.open_dir(Path::new(BASELINES_DIR), false) {
        Ok(dir) => dir,
        Err(ArtifactError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    read_regular_if_present(dir.as_raw_fd(), POINTER_NAME)
}

/// A candidate named the only way a caller may name one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateReference {
    pub run_id: String,
    pub file_name: String,
}

impl CandidateReference {
    /// Parse `<run-id>/<file-name>`, root-relative.
    ///
    /// Anything else is refused rather than interpreted: an absolute path, a
    /// deeper path, or a `..` are all ways of reading a file the restricted root
    /// was supposed to bound.
    pub fn parse(value: &str) -> ArtifactResult<Self> {
        let invalid = || ArtifactError::InvalidCandidateReference {
            value: value.to_string(),
        };
        let (run_id, file_name) = value.split_once('/').ok_or_else(invalid)?;
        if file_name.contains('/') {
            return Err(invalid());
        }
        validate_run_id(run_id).map_err(|_| invalid())?;
        let file_name = bare_name(file_name).map_err(|_| invalid())?;
        Ok(Self {
            run_id: run_id.to_string(),
            file_name,
        })
    }
}

/// Read a candidate produced by an earlier run.
pub fn read_candidate(
    root: &RestrictedRoot,
    reference: &CandidateReference,
) -> ArtifactResult<Vec<u8>> {
    let dir = root.open_dir(&Path::new(RUNS_DIR).join(&reference.run_id), false)?;
    read_regular_if_present(dir.as_raw_fd(), &reference.file_name)?.ok_or_else(|| {
        ArtifactError::io(
            "openat",
            reference.file_name.clone(),
            io::Error::from(io::ErrorKind::NotFound),
        )
    })
}

// ---------------------------------------------------------------- primitives

#[cfg(target_os = "linux")]
fn require_linux() -> ArtifactResult<()> {
    Ok(())
}

/// Refuse rather than degrade.
///
/// The atomicity this module promises rests on `RENAME_NOREPLACE`. Falling back
/// to "check then rename" elsewhere would keep the API working and quietly drop
/// the guarantee on the platforms where nobody looked.
#[cfg(not(target_os = "linux"))]
fn require_linux() -> ArtifactResult<()> {
    Err(ArtifactError::UnsupportedPlatform)
}

/// Retry a syscall that was interrupted by a signal.
///
/// Without this, a signal arriving at the wrong moment turns a perfectly valid
/// operation into a hard failure — and the caller cannot tell that from a real
/// refusal, which is the one distinction this module is built to make clear.
pub(crate) fn retry_on_interrupt(mut call: impl FnMut() -> libc::c_int) -> libc::c_int {
    loop {
        let result = call();
        if result >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return result;
        }
    }
}

/// Zero the thread's `errno` slot before a `readdir` call.
///
/// `readdir` reports both "end of directory" and "error" as NULL, so callers
/// distinguish them by clearing errno first. The thread-local accessor has a
/// different name per platform: `__errno_location` on Linux, `__error` on
/// macOS.
#[cfg(target_os = "linux")]
pub(crate) fn clear_errno() {
    // SAFETY: `__errno_location` returns a valid pointer for this thread.
    unsafe { *libc::__errno_location() = 0 };
}

/// See the Linux variant above.
#[cfg(not(target_os = "linux"))]
pub(crate) fn clear_errno() {
    // SAFETY: `__error` returns a valid pointer for this thread.
    unsafe { *libc::__error() = 0 };
}

fn retry_on_interrupt_isize(mut call: impl FnMut() -> libc::ssize_t) -> libc::ssize_t {
    loop {
        let result = call();
        if result >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return result;
        }
    }
}

fn cstring(path: &Path) -> ArtifactResult<CString> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| ArtifactError::PathEscape {
        component: path.display().to_string(),
    })
}

/// The only kind of path component this module accepts.
///
/// `..`, `/`, `.` and prefixes are refused instead of being normalised —
/// normalising is how an escape gets past a check that looked correct.
fn plain_name(component: &Component<'_>) -> ArtifactResult<String> {
    match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy().into_owned();
            if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                Err(ArtifactError::PathEscape { component: name })
            } else {
                Ok(name)
            }
        }
        other => Err(ArtifactError::PathEscape {
            component: format!("{other:?}"),
        }),
    }
}

/// A bare file name: no directory component, no traversal.
pub fn bare_name(name: &str) -> ArtifactResult<String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\0')
        || Path::new(name).components().count() != 1
    {
        return Err(ArtifactError::NotABareName {
            name: name.to_string(),
        });
    }
    Ok(name.to_string())
}

fn dup_fd(fd: RawFd, display: &Path) -> ArtifactResult<OwnedFd> {
    // SAFETY: `fd` is an open descriptor owned by the caller for the duration.
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(ArtifactError::io(
            "fcntl",
            display.display().to_string(),
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: a fresh descriptor this call owns.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn openat_dir(dir: RawFd, name: &str) -> ArtifactResult<OwnedFd> {
    let c_name = cstring(Path::new(name))?;
    // SAFETY: `dir` is an open directory fd; `c_name` outlives the call.
    let fd = retry_on_interrupt(|| unsafe {
        libc::openat(
            dir,
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    });
    if fd < 0 {
        return Err(ArtifactError::io(
            "openat",
            name.to_string(),
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: a fresh descriptor this call owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat_create_exclusive(dir: RawFd, name: &str) -> ArtifactResult<OwnedFd> {
    let c_name = cstring(Path::new(name))?;
    // SAFETY: `dir` is an open directory fd; `c_name` outlives the call.
    let fd = retry_on_interrupt(|| unsafe {
        libc::openat(
            dir,
            c_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    });
    if fd < 0 {
        return Err(ArtifactError::io(
            "openat",
            name.to_string(),
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: a fresh descriptor this call owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Read a regular file relative to `dir`, or `None` if it is not there.
///
/// The `O_NOFOLLOW` and the `S_ISREG` check are both load-bearing: the first
/// refuses a symlink planted in place of the artifact, the second refuses a
/// fifo or device that would otherwise make the read block or lie.
fn read_regular_if_present(dir: RawFd, name: &str) -> ArtifactResult<Option<Vec<u8>>> {
    let c_name = cstring(Path::new(name))?;
    // SAFETY: `dir` is an open directory fd; `c_name` outlives the call.
    // `O_NONBLOCK` is what keeps a planted FIFO from hanging the caller: the
    // `S_ISREG` check below is the refusal, but it only runs once the open
    // returns, and opening a FIFO for reading blocks until a writer appears.
    let fd = retry_on_interrupt(|| unsafe {
        libc::openat(
            dir,
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    });
    if fd < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            // `O_NOFOLLOW` reports a symlink as ELOOP; that is a refusal, not
            // an absence, and must not be reported as "no such artifact".
            _ => Err(ArtifactError::io("openat", name.to_string(), err)),
        };
    }
    // SAFETY: a fresh descriptor this call owns.
    let file = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `file` is open and `stat` is a valid out-parameter.
    if retry_on_interrupt(|| unsafe { libc::fstat(file.as_raw_fd(), &mut stat) }) < 0 {
        return Err(ArtifactError::io(
            "fstat",
            name.to_string(),
            io::Error::last_os_error(),
        ));
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(ArtifactError::NotARegularFile {
            path: name.to_string(),
        });
    }
    // A hardlink is the remaining way to read a file that lives outside the
    // root: `O_NOFOLLOW` stops symlinks, but a second name for an existing
    // inode is not a link to follow. Every artifact this module writes has
    // exactly one name, so more than one means the entry is an alias for
    // something this module did not create.
    //
    // This is defence in depth, not a boundary: a link count is mutable, so
    // dropping the outside name before the read leaves one link and passes.
    // The actual boundary is who can create names under the root at all —
    // anyone who can plant a hardlink there can equally write the file
    // outright, so the alias check buys detection of the careless case rather
    // than protection from the determined one.
    if stat.st_nlink != 1 {
        return Err(ArtifactError::AliasedFile {
            path: name.to_string(),
            links: stat.st_nlink as u64,
        });
    }

    let mut contents = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        // SAFETY: `file` is open; the buffer is valid for `buffer.len()` bytes.
        let read = retry_on_interrupt_isize(|| unsafe {
            libc::read(
                file.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len() as libc::size_t,
            )
        });
        if read < 0 {
            return Err(ArtifactError::io(
                "read",
                name.to_string(),
                io::Error::last_os_error(),
            ));
        }
        if read == 0 {
            break;
        }
        contents.extend_from_slice(&buffer[..read as usize]);
    }
    Ok(Some(contents))
}

fn write_all(fd: RawFd, mut contents: &[u8], name: &str) -> ArtifactResult<()> {
    while !contents.is_empty() {
        // SAFETY: `fd` is open for writing; the slice is valid for its length.
        let written =
            unsafe { libc::write(fd, contents.as_ptr().cast(), contents.len() as libc::size_t) };
        if written < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ArtifactError::io("write", name.to_string(), err));
        }
        if written == 0 {
            // Not an error the kernel reported, and not progress either.
            // Continuing would spin forever on a descriptor that has stopped
            // accepting bytes.
            return Err(ArtifactError::io(
                "write",
                name.to_string(),
                io::Error::from(io::ErrorKind::WriteZero),
            ));
        }
        contents = &contents[written as usize..];
    }
    Ok(())
}

fn fsync(fd: RawFd, name: &str) -> ArtifactResult<()> {
    // SAFETY: `fd` is an open descriptor.
    if retry_on_interrupt(|| unsafe { libc::fsync(fd) }) < 0 {
        return Err(ArtifactError::io(
            "fsync",
            name.to_string(),
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn renameat(dir: RawFd, from: &str, to: &str) -> Result<(), io::Error> {
    let c_from = CString::new(from).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let c_to = CString::new(to).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `dir` is an open directory fd; both names outlive the call.
    let result =
        retry_on_interrupt(|| unsafe { libc::renameat(dir, c_from.as_ptr(), dir, c_to.as_ptr()) });
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `renameat2(RENAME_NOREPLACE)` via the raw syscall.
///
/// Called through `syscall` rather than a libc wrapper because the wrapper is
/// not exposed on every target this builds for, and the flag is the whole point
/// of the call.
#[cfg(target_os = "linux")]
fn rename_noreplace(dir: RawFd, from: &str, to: &str) -> Result<(), io::Error> {
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let c_from = CString::new(from).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let c_to = CString::new(to).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `dir` is an open directory fd; both names outlive the call; the
    // argument shape matches renameat2(2).
    let result = retry_on_interrupt_isize(|| unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            dir,
            c_from.as_ptr(),
            dir,
            c_to.as_ptr(),
            RENAME_NOREPLACE,
        ) as libc::ssize_t
    });
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_dir: RawFd, _from: &str, _to: &str) -> Result<(), io::Error> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

fn unlinkat(dir: RawFd, name: &str) {
    if let Ok(c_name) = CString::new(name) {
        // SAFETY: `dir` is an open directory fd; `c_name` outlives the call.
        unsafe { libc::unlinkat(dir, c_name.as_ptr(), 0) };
    }
}

// ---------------------------------------------------------------- run ids

/// `<UTC timestamp>-<random digits>`.
///
/// The timestamp is for a human reading a directory listing; the random suffix
/// is what makes two runs in the same second distinguishable. Neither is a
/// uniqueness guarantee on its own — the `mkdirat` is.
pub fn generate_run_id() -> String {
    use rand::RngExt;

    let now = chrono::Utc::now();
    let suffix: u32 = rand::rng().random_range(1..1_000_000);
    format!("{}-{}", now.format("%Y%m%dT%H%M%SZ"), suffix)
}

/// `^[0-9]{8}T[0-9]{6}Z-[0-9]+$`, checked without a regex dependency.
pub fn validate_run_id(value: &str) -> ArtifactResult<()> {
    let invalid = || ArtifactError::InvalidRunId {
        value: value.to_string(),
    };
    let (stamp, suffix) = value.split_once('-').ok_or_else(invalid)?;
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let bytes = stamp.as_bytes();
    if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
        return Err(invalid());
    }
    if !bytes[..8].iter().all(u8::is_ascii_digit) || !bytes[9..15].iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    Ok(())
}

fn random_suffix() -> String {
    use rand::RngExt;
    format!("{:016x}", rand::rng().random::<u64>())
}

#[cfg(all(test, target_os = "linux"))]
mod artifact_writer {
    //! UN-32: the restricted artifact writer, tested where it matters.
    //!
    //! The interesting failures here are not "does the file appear" but "can a
    //! caller, or something that got there first, make the file appear somewhere
    //! else". So most of these tests are about refusals: a symlink planted in the
    //! path, a name with a directory in it, a run id supplied from outside, a
    //! version file that already exists with different content.
    //!
    //! Every refusal is paired with the corresponding success, because a writer
    //! that refuses everything would pass a suite made only of refusals.

    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    use crate::contract::policy::secure_artifact::{
        ArtifactError, BASELINES_DIR, CandidateReference, POINTER_NAME, RUNS_DIR, RestrictedRoot,
        RunDir, bare_name, generate_run_id, read_candidate, read_pointer, replace_pointer,
        validate_run_id, write_baseline_version,
    };

    fn root() -> (tempfile::TempDir, RestrictedRoot) {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = RestrictedRoot::open(temp.path()).expect("open the restricted root");
        (temp, root)
    }

    /// No root, no writer.
    ///
    /// The CLI flag belongs to the command card; what belongs here is that there is
    /// no way to obtain a writer without naming a root, so a caller cannot end up
    /// writing to a default location nobody registered.
    #[test]
    fn un32_a_writer_cannot_be_obtained_without_a_root() {
        let error =
            RestrictedRoot::from_option(None::<&Path>).expect_err("no root must be refused");
        assert!(matches!(error, ArtifactError::RestrictedRootMissing));
        assert!(
            error.to_string().contains("--restricted-root is required"),
            "the message must name the missing argument: {error}"
        );

        let temp = tempfile::tempdir().expect("temp dir");
        RestrictedRoot::from_option(Some(temp.path())).expect("a named root opens");
    }

    /// A root that is itself a symlink is refused.
    ///
    /// "The root" would otherwise mean whatever the link points at today, which is
    /// exactly the property an operator registering a directory is trying to fix.
    #[test]
    fn un32_a_symlinked_root_is_refused() {
        let temp = tempfile::tempdir().expect("temp dir");
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).expect("create the real root");
        std::os::unix::fs::symlink(&real, &link).expect("create the symlink");

        let error = RestrictedRoot::open(&link).expect_err("a symlinked root must be refused");
        assert!(matches!(error, ArtifactError::RootOpen { .. }), "{error}");

        RestrictedRoot::open(&real).expect("the real directory opens");
    }

    /// Run outputs land in the run directory, at 0600, with the frozen id line.
    #[test]
    fn un32_a_run_writes_its_outputs_at_0600() {
        let (temp, root) = root();
        let run = RunDir::create(&root).expect("claim a run directory");

        validate_run_id(run.run_id()).expect("the generated id must satisfy its own syntax");
        assert_eq!(run.run_id_line(), format!("run_id={}", run.run_id()));

        run.write_output("report.json", b"{\"ok\":true}")
            .expect("write the report");

        let path = temp
            .path()
            .join(RUNS_DIR)
            .join(run.run_id())
            .join("report.json");
        assert_eq!(
            fs::read(&path).expect("read the report"),
            b"{\"ok\":true}".to_vec()
        );
        assert_eq!(
            fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o600,
            "a restricted artifact must not be readable by anyone else"
        );
    }

    /// Two runs never share a directory.
    #[test]
    fn un32_two_runs_get_two_directories() {
        let (temp, root) = root();
        let first = RunDir::create(&root).expect("first run");
        let second = RunDir::create(&root).expect("second run");

        assert_ne!(first.run_id(), second.run_id());
        for id in [first.run_id(), second.run_id()] {
            assert!(temp.path().join(RUNS_DIR).join(id).is_dir());
        }
    }

    /// A colliding id is retried, and the retry is bounded.
    ///
    /// The `mkdirat` is what makes a run directory unique — not the timestamp and
    /// not the random suffix — so the collision path is the one that has to be
    /// right. Feeding a fixed id is the only way to reach it; waiting for a real
    /// clock collision is not a test.
    #[test]
    fn un32_a_colliding_run_id_is_retried_and_then_gives_up() {
        let (_temp, root) = root();

        // One collision, then a fresh id: the claim must succeed.
        let mut issued = 0;
        let taken = RunDir::create(&root)
            .expect("occupy an id")
            .run_id()
            .to_string();
        let recovered = RunDir::create_with(&root, || {
            issued += 1;
            if issued == 1 {
                taken.clone()
            } else {
                generate_run_id()
            }
        })
        .expect("a single collision must be retried, not fatal");
        assert_ne!(recovered.run_id(), taken);
        assert_eq!(issued, 2, "exactly one retry was needed");

        // A generator that always collides must stop, not spin.
        let mut attempts = 0;
        let error = RunDir::create_with(&root, || {
            attempts += 1;
            taken.clone()
        })
        .expect_err("an id that always collides must fail");
        assert!(
            matches!(error, ArtifactError::RunIdExhausted { attempts: 5 }),
            "{error}"
        );
        assert_eq!(attempts, 5, "the retry bound must actually bound the loop");
    }

    /// A run id supplied from outside is not a run id.
    #[test]
    fn un32_a_supplied_run_id_must_still_satisfy_the_syntax() {
        for bad in [
            "not-a-run-id",
            "20260815T101112Z",      // no suffix
            "20260815T101112Z-",     // empty suffix
            "20260815T101112Z-abc",  // non-numeric suffix
            "2026081T101112Z-1",     // short date
            "20260815X101112Z-1",    // wrong separator
            "../20260815T101112Z-1", // traversal dressed as an id
        ] {
            assert!(
                validate_run_id(bad).is_err(),
                "`{bad}` must not be accepted as a run id"
            );
        }
        validate_run_id("20260815T101112Z-1").expect("the frozen syntax must be accepted");
        validate_run_id(&generate_run_id()).expect("generated ids must satisfy it");
    }

    /// Output names are bare names.
    ///
    /// Accepting a path here would let the caller pick a directory, which is the
    /// one decision the restricted layout exists to make.
    #[test]
    fn un32_an_output_name_with_a_directory_component_is_refused() {
        let (_temp, root) = root();
        let run = RunDir::create(&root).expect("claim a run directory");

        for bad in [
            "../escape.json",
            "nested/report.json",
            "/absolute.json",
            "..",
            ".",
            "",
        ] {
            let error = run
                .write_output(bad, b"x")
                .err()
                .unwrap_or_else(|| panic!("`{bad}` must be refused as an output name"));
            assert!(
                matches!(error, ArtifactError::NotABareName { .. }),
                "`{bad}`: {error}"
            );
        }

        run.write_output("fine.json", b"x")
            .expect("a bare name is accepted");
        assert!(bare_name("fine.json").is_ok());
    }

    /// A symlink planted where the artifact goes is refused, not followed.
    ///
    /// This is the attack the whole module is shaped around: something that can
    /// create a name inside the run directory before the writer gets there should
    /// not be able to redirect the write outside the root.
    #[test]
    fn un32_a_symlink_in_place_of_an_output_is_refused() {
        let (temp, root) = root();
        let run = RunDir::create(&root).expect("claim a run directory");
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"original").expect("seed the outside file");

        let planted = temp
            .path()
            .join(RUNS_DIR)
            .join(run.run_id())
            .join("report.json");
        std::os::unix::fs::symlink(&outside, &planted).expect("plant the symlink");

        let error = run
            .write_output("report.json", b"redirected")
            .expect_err("writing through a planted symlink must be refused");
        assert!(matches!(error, ArtifactError::Io { .. }), "{error}");
        assert_eq!(
            fs::read(&outside).expect("read the outside file"),
            b"original".to_vec(),
            "the refused write must not have reached the symlink target"
        );
    }

    /// A symlink standing in for a directory on the way down is refused too.
    #[test]
    fn un32_a_symlinked_directory_component_is_refused() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root_dir = temp.path().join("root");
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir(&root_dir).expect("create the root");
        fs::create_dir(&elsewhere).expect("create the other directory");
        // `runs` is a symlink pointing outside the root before anyone writes.
        std::os::unix::fs::symlink(&elsewhere, root_dir.join(RUNS_DIR)).expect("plant the symlink");

        let root = RestrictedRoot::open(&root_dir).expect("open the root");
        let error = RunDir::create(&root).expect_err("a symlinked runs/ directory must be refused");
        assert!(matches!(error, ArtifactError::Io { .. }), "{error}");
        assert!(
            fs::read_dir(&elsewhere)
                .expect("read the other directory")
                .next()
                .is_none(),
            "nothing may have been created through the symlink"
        );
    }

    /// Writing the same baseline version twice is fine; writing a different one is
    /// not.
    ///
    /// A version file is named after the digest of its content, so a name arriving
    /// twice with different bytes means the name has stopped meaning what it says —
    /// and silently replacing it would rewrite history the promotion chain depends
    /// on.
    #[test]
    fn un32_a_baseline_version_is_immutable_once_written() {
        let (temp, root) = root();

        write_baseline_version(&root, "abc123.json", b"first").expect("write the version");
        let path = temp.path().join(BASELINES_DIR).join("abc123.json");
        assert_eq!(fs::read(&path).expect("read"), b"first".to_vec());
        assert_eq!(
            fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o600
        );

        write_baseline_version(&root, "abc123.json", b"first")
            .expect("re-writing identical content is a no-op, not an error");

        let error = write_baseline_version(&root, "abc123.json", b"different")
            .expect_err("different content under the same name must be refused");
        assert!(
            matches!(error, ArtifactError::BaselineVersionConflict { .. }),
            "{error}"
        );
        assert_eq!(
            fs::read(&path).expect("read"),
            b"first".to_vec(),
            "the refused write must have left the original in place"
        );
    }

    /// No temporary files are left behind by a successful version write.
    #[test]
    fn un32_writing_a_version_leaves_no_temporary_behind() {
        let (temp, root) = root();
        write_baseline_version(&root, "v1.json", b"content").expect("write the version");

        let leftovers: Vec<String> = fs::read_dir(temp.path().join(BASELINES_DIR))
            .expect("list baselines")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    /// The pointer is the one artifact that is meant to be replaced.
    #[test]
    fn un32_the_pointer_is_replaced_atomically_and_read_back_from_a_derived_path() {
        let (temp, root) = root();

        assert!(
            read_pointer(&root)
                .expect("read a missing pointer")
                .is_none(),
            "no pointer yet is an absence, not an error"
        );

        replace_pointer(&root, b"{\"version\":1}").expect("write the pointer");
        assert_eq!(
            read_pointer(&root).expect("read").expect("present"),
            b"{\"version\":1}".to_vec()
        );

        replace_pointer(&root, b"{\"version\":2}").expect("replace the pointer");
        assert_eq!(
            read_pointer(&root).expect("read").expect("present"),
            b"{\"version\":2}".to_vec(),
            "replacing is the pointer's whole purpose"
        );

        let path = temp.path().join(BASELINES_DIR).join(POINTER_NAME);
        assert_eq!(
            fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o600
        );
    }

    /// A symlink standing in for the pointer is refused on read.
    ///
    /// Reported as a refusal rather than as "no pointer": treating it as an absence
    /// would let someone turn a pinned baseline into an unpinned one by planting a
    /// dangling link.
    #[test]
    fn un32_a_symlinked_pointer_is_refused_rather_than_read_as_absent() {
        let (temp, root) = root();
        let baselines = temp.path().join(BASELINES_DIR);
        fs::create_dir_all(&baselines).expect("create baselines");
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"{\"version\":99}").expect("seed");
        std::os::unix::fs::symlink(&outside, baselines.join(POINTER_NAME)).expect("plant");

        let error = read_pointer(&root).expect_err("a symlinked pointer must be refused");
        assert!(matches!(error, ArtifactError::Io { .. }), "{error}");
    }

    /// A candidate is named `<run-id>/<file-name>` and nothing else.
    #[test]
    fn un32_a_candidate_reference_is_root_relative_and_two_components() {
        let parsed = CandidateReference::parse("20260815T101112Z-7/candidate.json")
            .expect("the frozen form is accepted");
        assert_eq!(parsed.run_id, "20260815T101112Z-7");
        assert_eq!(parsed.file_name, "candidate.json");

        for bad in [
            "/absolute/candidate.json",
            "../20260815T101112Z-7/candidate.json",
            "20260815T101112Z-7/nested/candidate.json",
            "20260815T101112Z-7/../../escape.json",
            "not-a-run-id/candidate.json",
            "candidate.json",
            "20260815T101112Z-7/",
        ] {
            assert!(
                CandidateReference::parse(bad).is_err(),
                "`{bad}` must not parse as a candidate reference"
            );
        }
    }

    /// Reading a candidate goes through the root, and through the same refusals.
    #[test]
    fn un32_reading_a_candidate_uses_the_root_and_refuses_a_symlink() {
        let (temp, root) = root();
        let run = RunDir::create(&root).expect("claim a run directory");
        run.write_output("candidate.json", b"{\"candidate\":true}")
            .expect("write the candidate");

        let reference =
            CandidateReference::parse(&format!("{}/candidate.json", run.run_id())).expect("parse");
        assert_eq!(
            read_candidate(&root, &reference).expect("read the candidate"),
            b"{\"candidate\":true}".to_vec()
        );

        // A candidate name pointing at a planted symlink is refused.
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"elsewhere").expect("seed");
        let planted = temp
            .path()
            .join(RUNS_DIR)
            .join(run.run_id())
            .join("planted.json");
        std::os::unix::fs::symlink(&outside, &planted).expect("plant");
        let planted_reference =
            CandidateReference::parse(&format!("{}/planted.json", run.run_id())).expect("parse");
        assert!(
            read_candidate(&root, &planted_reference).is_err(),
            "a symlinked candidate must be refused, not followed outside the root"
        );
    }

    /// A candidate that is not a regular file is refused.
    #[test]
    fn un32_a_candidate_that_is_not_a_regular_file_is_refused() {
        let (temp, root) = root();
        let run = RunDir::create(&root).expect("claim a run directory");
        fs::create_dir(
            temp.path()
                .join(RUNS_DIR)
                .join(run.run_id())
                .join("directory.json"),
        )
        .expect("create a directory where a file is expected");

        let reference =
            CandidateReference::parse(&format!("{}/directory.json", run.run_id())).expect("parse");
        let error = read_candidate(&root, &reference)
            .expect_err("a directory must not be read as a candidate");
        assert!(
            matches!(
                error,
                ArtifactError::NotARegularFile { .. } | ArtifactError::Io { .. }
            ),
            "{error}"
        );
    }

    /// The platform rule, stated where it is enforced.
    ///
    /// On Linux this is the positive case; the refusal on other platforms is the
    /// same `require_linux` gate at the head of `RestrictedRoot::open`, so there is
    /// one place to read rather than a rule repeated per entry point.
    #[test]
    fn un32_the_platform_gate_admits_linux_and_names_the_reason_otherwise() {
        let temp = tempfile::tempdir().expect("temp dir");
        let opened = RestrictedRoot::open(temp.path());

        if cfg!(target_os = "linux") {
            opened.expect("Linux is the supported platform");
        } else {
            let error = opened.expect_err("other platforms must fail closed");
            assert!(matches!(error, ArtifactError::UnsupportedPlatform));
            assert!(
                error
                    .to_string()
                    .contains("Run the audit from a Linux host"),
                "the refusal must be actionable: {error}"
            );
        }
    }

    /// A hardlink standing in for an artifact is refused.
    ///
    /// `O_NOFOLLOW` stops a symlink, but a second name for an existing inode is not
    /// a link to follow — it simply *is* the outside file, under a name inside the
    /// root. Every artifact this module writes has exactly one name, so more than
    /// one is the signature of an entry it did not create.
    #[test]
    fn un32_a_hardlinked_artifact_is_refused() {
        let (temp, root) = root();
        let run = RunDir::create(&root).expect("claim a run directory");

        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"{\"secret\":true}").expect("seed the outside file");
        let inside = temp
            .path()
            .join(RUNS_DIR)
            .join(run.run_id())
            .join("candidate.json");
        fs::hard_link(&outside, &inside).expect("plant the hardlink");

        let reference =
            CandidateReference::parse(&format!("{}/candidate.json", run.run_id())).expect("parse");
        let error =
            read_candidate(&root, &reference).expect_err("a hardlinked candidate must be refused");
        assert!(
            matches!(error, ArtifactError::AliasedFile { links: 2, .. }),
            "{error}"
        );

        // The control: the same read succeeds for a file this module wrote, so the
        // refusal is about the alias rather than about reads being broken.
        run.write_output("real.json", b"{\"real\":true}")
            .expect("write a real candidate");
        let real =
            CandidateReference::parse(&format!("{}/real.json", run.run_id())).expect("parse");
        assert_eq!(
            read_candidate(&root, &real).expect("read the real candidate"),
            b"{\"real\":true}".to_vec()
        );
    }

    /// A hardlinked pointer is refused on the same grounds.
    #[test]
    fn un32_a_hardlinked_pointer_is_refused() {
        let (temp, root) = root();
        let baselines = temp.path().join(BASELINES_DIR);
        fs::create_dir_all(&baselines).expect("create baselines");
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"{\"version\":99}").expect("seed");
        fs::hard_link(&outside, baselines.join(POINTER_NAME)).expect("plant the hardlink");

        let error = read_pointer(&root).expect_err("a hardlinked pointer must be refused");
        assert!(
            matches!(error, ArtifactError::AliasedFile { .. }),
            "{error}"
        );
    }

    /// A FIFO planted where an artifact belongs is refused, and refused promptly.
    ///
    /// The regular-file check is the refusal, but it only runs once the open
    /// returns — and opening a FIFO for reading blocks until a writer shows up. A
    /// caller that hangs forever is a worse outcome than one that reads the wrong
    /// file, because nobody gets an error to act on.
    #[test]
    fn un32_a_fifo_in_place_of_an_artifact_is_refused_without_blocking() {
        use std::{ffi::CString, time::Instant};

        let (temp, root) = root();
        let run = RunDir::create(&root).expect("claim a run directory");
        let fifo = temp
            .path()
            .join(RUNS_DIR)
            .join(run.run_id())
            .join("candidate.json");
        let c_path = CString::new(fifo.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(
            unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) },
            0,
            "plant the fifo"
        );

        let reference =
            CandidateReference::parse(&format!("{}/candidate.json", run.run_id())).expect("parse");
        let started = Instant::now();
        let error = read_candidate(&root, &reference).expect_err("a fifo must be refused");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the refusal must not wait for a writer that never comes"
        );
        assert!(
            matches!(error, ArtifactError::NotARegularFile { .. }),
            "{error}"
        );
    }

    /// Reading a pointer that is not there creates nothing.
    ///
    /// A read path that makes a directory has written to the root, which is the
    /// posture the read rules exist to hold. A missing `baselines/` is the same
    /// answer as a missing pointer, not an invitation to build one.
    #[test]
    fn un32_reading_a_missing_pointer_creates_nothing() {
        let (temp, root) = root();

        assert!(
            read_pointer(&root)
                .expect("a missing pointer reads as absent")
                .is_none()
        );
        assert!(
            !temp.path().join(BASELINES_DIR).exists(),
            "the read must not have created the baselines directory"
        );

        // The control: writing is what creates it, so the absence above is a
        // property of the read rather than of nothing ever creating the directory.
        replace_pointer(&root, b"{}").expect("write the pointer");
        assert!(temp.path().join(BASELINES_DIR).is_dir());
        assert!(read_pointer(&root).expect("read").is_some());
    }
}
