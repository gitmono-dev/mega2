# FIX-OX-15 evidence

State: the card-specific A/B test, formatting, and clippy gates pass. Claude ER-05 round 2 returned `VERDICT: PASS`; the card is locally accepted. The repository-wide test command is still running, so the full test/build evidence must be finalized before push.

## Scope and provenance

The base is `1adbfef0e62eb99e9707253cbfe66ee49f3fe0cd`. `A-source.rs` matches the base blob for `src/api/router/snapshot_persisted_chunk_map_tests.rs`; the B source hash matches the current working file. The unified diff has two test-only hunks: a more informative owner-timeout assertion and repository/budget/fixture wiring. `libra diff -- src` confirms the only tracked source change is the plan-owned test file.

The final test installs a real `PostgresChunkMapRepository` and an 8 MiB test budget before the held leader, then uses actual HTTP router requests. Base/current SHA-256 checks in `verification/source-restoration-check.json` show the production source files that appeared in temporary diagnostic snapshots are byte-identical to the base. The existing `Fixture::new_without_publication` helper is also present in the base `snapshot_content_tests.rs`.

## Acceptance evidence

- AC-1: repository and memory budget are installed before `held_leader`; budget use is checked after receipt-write admission.
- AC-2: same-source callers are created after the receipt-write gate is held.
- AC-3: the test waits for all owners, asserts one whole read and one receipt write, compares each same-source response, requires eight receipt reads, and verifies owners drain.
- AC-4: another storage's injected receipt-read failure returns 502 `INTEGRITY_ERROR`, with one receipt read and zero whole reads.

## Verification

- A baseline VER-1: exit 101 at the install-gate owner timeout.
- Publication-enabled intermediate B: repository and budget installed, receipt-write gate reached, but seven callers timed out in rooted metadata route admission. `J-debug.stdout` records PostgreSQL `mst2_route_enter` advisory-lock waits. The files are explicitly named `B-publication-enabled-VER-1.*` under `verification/diagnostic/published-fixture-attempt/`.
- Final B VER-1: exit 0; `1 passed; 0 failed; 2463 filtered out`.
- `cargo +nightly fmt --all --check`: exit 0.
- `cargo clippy --all-targets --all-features -- -D warnings`: exit 0.
- Repository-wide `source .env.test && cargo test --all` is still running separately and has reported failures before completion; build and build-tests remain pending. These results will be recorded before the card push.
- The macOS linker warning remains assigned to OX-284 final C.

## Claude review round 2

Claude Code returned literal `VERDICT: PASS` with no P0 or P1 findings. The round-1 P1 restoration proof is closed. The route-admission contention remains a non-blocking FIX-OX-18 follow-up; its execution ledger must include a publication-enabled rerun of this test and capture the early caller's HTTP status/receipt-read behavior. The repository-wide test/build gates remain separate from card acceptance and must be completed, with failures attributed, before push.

The publication-disabled final fixture isolates the chunk-map behavior while retaining the actual HTTP path and PostgreSQL repository. The route-admission contention remains unresolved and is assigned to the existing FIX-OX-18 investigation; this assignment does not presume a test-hook or production defect. The task card's Current evidence is the frozen base observation; this A/B diagnosis is recorded in this evidence and the live `plan-status.md` ledger.

Intermediate instrumented source snapshots are preserved under `verification/diagnostic/` only as diagnostics. `verification/source-restoration-check.json` records their original source mappings and proves the current production files match the base. Their emitted `#[cfg(test)]` diagnostics are not in the final source.

## Release and push instruction

The user updated execution policy on 2026-10-10: after each task card passes review and is committed, its commit may be pushed. Version bump, tag, and GitHub Release remain reserved for OX-284. This later user instruction governs execution despite frozen no-push wording. This card has not yet been committed or pushed.
