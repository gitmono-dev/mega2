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
    let error = RestrictedRoot::from_option(None::<&Path>).expect_err("no root must be refused");
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
    let error =
        read_candidate(&root, &reference).expect_err("a directory must not be read as a candidate");
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
    let real = CandidateReference::parse(&format!("{}/real.json", run.run_id())).expect("parse");
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
