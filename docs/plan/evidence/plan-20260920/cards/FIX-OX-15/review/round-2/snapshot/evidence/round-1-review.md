Review of FIX-OX-15 round 1, performed read-only against the inline packet (manifest SHA `3852b412…`).

## Provenance and scope

- **A matches base.** The A source hash `17ece762…` equals the manifest's `libra show 1adbfef0…:<test file>` digest and the snapshot manifest entry. **B matches the working source** (`831d82f6…`, `matches_worktree_source: true`). The diff hash `f562185a…` is consistent across manifest and snapshot.
- **Diff content is test-only within the planned file.** Hunk 1 (`code/A-B-source.diff` lines 6-13) only improves the `wait_owners` panic message. Hunk 2 (lines 18-45) switches the fixture constructor, installs the PostgreSQL repository with an 8 MiB budget, and asserts budget use after the gate is entered.
- **Version face unchanged.** Both B runs compile `mega2 v0.42.25`, matching the plan-status version. No tag or release artifact appears anywhere in the packet.

## Findings

**P1 (blocking). The packet cannot show that production sources were restored after diagnostic instrumentation.** The diagnostic artifact list (`evidence/ab-manifest.json`) includes instrumented copies of production files: `E-debug-content-source.rs`, `E-debug-router-source.rs`, `F-/G-debug-content-source.rs`, `H-debug-reader-source.rs`, and `I-debug-fixture-source.rs`. `evidence/J-debug.stdout` shows `eprintln!` output emitted from production code paths ("request auth start", "rooted admit_reader start", "project flight acquired"). Neither fmt nor clippy proves those lines were removed, and `B-final-VER-1.stdout` cannot prove it either because libtest captures output of passing tests. The A/B manifest diffs only the one test file by construction. There is no `libra status --short --branch` output, no diff-stat, and no base-versus-worktree hash for the instrumented files. Items 1 and 5 of this review therefore cannot be confirmed. Required for round 2: sanitized `libra status --short --branch` showing only the planned write set dirty, and base-versus-worktree hashes for every production file that appears in `verification/diagnostic/`. Also show that `Fixture::new_without_publication` exists in the base commit's fixture source, since the fixture file was itself instrumented and is outside the declared write set.

**P2 (non-blocking, needs written acceptance and a follow-up card). The root cause recorded on the card is incomplete, and the publication switch hides an unexplained production-path contention.** The intermediate B run (`evidence/B-VER-1.*`, publication enabled, repository installed) still failed with the same owner timeout. `evidence/J-debug.stdout` shows the leader did reach the receipt-write hold (`whole=1, receipt_writes=1, owners=2`) while all seven callers were stuck for over ten seconds in `mst2_route_enter` advisory-lock waits behind a `mst2_metadata_begin_reader` backend. So the card's stated cause (repository not installed) was only half the story. Switching to the publication-disabled fixture is defensible for this card's axis, but it means the test no longer covers same-source sharing under the default published configuration. Required: correct the card's Current evidence to record both causes, and register the seven-reader route-admission contention as a FIX or DEFER item before commit. This is not a fake path (see below), so I do not block on it.

**P2 (non-blocking). Repository-wide gate state must be recorded, not left as "running".** `verification-results.json` lists `cargo test --all` as running with failures observed, and `cargo build` / `cargo build --tests` as not run. Per ER-04 a plan release child self-runs A plus fmt and clippy, and inherits C from OX-284. So the unfinished gate does not block ER-05 review or `locally-accepted`. It does not block the local commit. It does block final submission at OX-284. Before the commit is pushed under the user's 2026-10-10 instruction, the evidence should record the completed run's failing test names and state whether any failure is attributable to this card's diff. `cargo build` and `cargo build --tests` are cheap and required by AGENTS.md for any `src/` change; run them.

**P3 observations.**
- `ab-manifest.json` says `hunk_count: 2` but its scope text and the README say "one hunk". Align the wording.
- `evidence/B-VER-1.*` is the intermediate failing run but is named like the final. The README should label it as the published-fixture attempt.
- `context/task-card.md` begins with trailing fields from the previous card and shows `Lifecycle=pending` and unchecked AC boxes. The commit must update the card and `plan-20260920.md`, which are in the write set but absent from the packet.
- `plan-status.md` repeats the same FIX-OX-13/14/15 sentences several times within single cells. Pre-existing, outside this write set, but worth cleaning at the next status update.
- The `wait_owners` message change is outside the stated AC but harmless and improves diagnosis.

## Fixture validity and AC-1..AC-4

The publication-disabled fixture still drives the real HTTP path: the leader and six callers go through `fixture.app`, the rejected caller through `router(&other_state)`, and the repository is a `PostgresChunkMapRepository` on the fixture's live connection. The budget assertion after `entered` proves the installed repository is the one charged. Not a weaker fake path, with the coverage caveat in the P2 above.

- **AC-1 proven.** Repository and budget installed before `held_leader` (B ~495-515); `set(...).is_ok()` proves no prior install; `budget.used() > 4 MiB` after the gate.
- **AC-2 proven.** All seven callers and the observer are spawned only after `entered.notified()` completes.
- **AC-3 proven.** `wait_owners(9)` then `counts.assert(1, raw.len())` before and after release, every caller's map equals the leader's, `receipt_writes == 1`, `receipt_reads == 8`, and `wait_owners(1)` confirms release.
- **AC-4 proven.** Other storage with injected receipt-read failure returns 502 `INTEGRITY_ERROR`, with `receipt_reads == 1` and `whole == 0` on its own counters.

## Commands, exits, hashes, redactions

A and B-final use the exact card VER-1 command. Exits 101 / 0 as claimed; exit-file digests are the correct SHA-256 of `101\n`, `0\n`, `1\n`. fmt and clippy exit 0 with empty stdout (empty-file digest). Sanitized stderr digests match between `redactions.json`, `verification-results.json`, and the snapshot manifest. Redaction of execution paths is present where paths occurred. No secrets or tokens observed.

VERDICT: FAIL
