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
pub fn replace_pointer(root: &RestrictedRoot, contents: &[u8]) -> ArtifactResult<()> {
    let dir = root.open_dir(Path::new(BASELINES_DIR), true)?;
    let temp = format!(".tmp-{}", random_suffix());

    let file = openat_create_exclusive(dir.as_raw_fd(), &temp)?;
    write_all(file.as_raw_fd(), contents, &temp)?;
    fsync(file.as_raw_fd(), &temp)?;
    drop(file);

    if let Err(err) = renameat(dir.as_raw_fd(), &temp, POINTER_NAME) {
        unlinkat(dir.as_raw_fd(), &temp);
        return Err(ArtifactError::io("renameat", POINTER_NAME, err));
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
