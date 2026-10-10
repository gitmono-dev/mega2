# FIX-OX-15 evidence

State: A/B, formatting, and clippy have passed. Claude Code ER-05 review is the remaining local acceptance gate. No commit has been made yet.

## Scope and provenance

This card changes only `src/api/router/snapshot_persisted_chunk_map_tests.rs`. The base is `1adbfef0e62eb99e9707253cbfe66ee49f3fe0cd`; `libra show <base>:<source>` hashes to the same value as `A-source.rs`. The final B source hash matches the working file. The A/B manifest and unified diff bind both sources and the exact test-only hunk.

The implementation installs `PostgresChunkMapRepository` and its test `MemoryBudget` before starting the held HTTP leader. The final test uses the actual router and endpoint. The publication-disabled fixture avoids unrelated rooted-metadata route-admission contention observed in the publication-enabled diagnostic variant; retained `J-debug.stdout` evidence includes PostgreSQL `mst2_route_enter` advisory-lock waits. Claude review should assess whether this fixture still exercises all required behavior and whether each acceptance assertion is sufficient.

## Acceptance evidence

- AC-1: the PostgreSQL chunk-map repository and 8 MiB test budget are installed before `held_leader`; the test verifies budget use after receipt-write admission.
- AC-2: callers are created only after the held leader reaches the receipt-write gate.
- AC-3: the test waits for all owners, asserts one whole read and one receipt write, compares each same-source response, and requires eight receipt reads including the leader.
- AC-4: an independent storage's injected receipt-read fault returns 502 `INTEGRITY_ERROR`; it performs one receipt read and zero whole reads.

## Verification

- A baseline: focused VER-1 exit 101 at the install-gate owner timeout.
- B final: exact focused VER-1 command passed, 1 test passed and 0 failed.
- `cargo +nightly fmt --all --check`: exit 0.
- `cargo clippy --all-targets --all-features -- -D warnings`: exit 0.
- The repository-wide test and build checks required by `AGENTS.md` are still pending and will be recorded before local acceptance.
- The macOS linker warning is retained and assigned to the plan's OX-284 final C gate.

Raw logs and source snapshots are in `verification/`; intermediate instrumented attempts are preserved under `verification/diagnostic/` and are not the final B source.

## Release and push instruction

Version bump, tag, and GitHub Release remain reserved for OX-284. The user updated execution policy on 2026-10-10: after a task card passes review and is committed, its commit may be pushed. That later user instruction governs this execution even though the frozen card text describes a no-push-until-OX-284 boundary. This card has not yet been committed or pushed.
