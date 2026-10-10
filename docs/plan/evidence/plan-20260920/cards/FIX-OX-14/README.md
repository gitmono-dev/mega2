# FIX-OX-14 evidence

State: the focused A/B test, `cargo +nightly fmt --all --check`, and `cargo clippy --all-targets --all-features -- -D warnings` passed. Claude Code ER-05 round 1 returned literal `VERDICT: PASS`, with four non-blocking P3 findings. The source change is already present in the frozen checkpoint; this card records its own A/B acceptance and review.

## Scope and frozen plan context

The owned hunk in `src/api/router/snapshot_rooted_metadata_tests.rs` checks that a GET for a non-current verified file fact returns 503 `METADATA_NOT_READY` with `retryable=true`, while the independent HEAD `/empty` check still returns 200 and zero content size, and the request reads zero body bytes. The production mapping already marks `MetadataNotReady` retryable. The B source is present at base commit `ac7acfdba08a427bc295be71ec5f2f465a515e32` and in the earlier checkpoint; this card performs card-specific A/B verification and review of that checkpoint-owned hunk. No production behavior changed.

The plan's `Current evidence` sentence is a frozen checkpoint observation: its 35.93s result predates this card's B-VER-1 run (34.18s). The plan's card fields remain their frozen defaults; the plan explicitly places live `Lifecycle / Acceptance` state in `docs/plan/plan-status.md`. The first review's state finding is therefore addressed in the status ledger, without modifying the frozen plan.

## A/B and type-gate results

- A changes only the target expectation from `true` to `false`; the focused test exited 101 because actual retryable was true (harness 32.27s).
- B retains `true`; the focused test exited 0 (`1 passed; 0 failed; 2463 filtered out`, harness 34.18s).
- `cargo +nightly fmt --all --check`: exit 0.
- `cargo clippy --all-targets --all-features -- -D warnings`: exit 0, bound to the B source hash and base commit.
- The A/B diff is one test-only hunk. The raw `libra show` output, stderr, and exit for the B test source and production mapping are retained. `production-mapping-check.json` binds the mapping excerpt to the base source hash and exact line range.
- Absolute checkout paths in test/clippy stderr were replaced with `[REDACTED_EXECUTION_PATH]`; original and sanitized hashes are in `redactions.json`.

The test linker emitted macOS `__eh_frame section too large`; keep it open for OX-284's final C build gate.

## ER-05 round 1

Report SHA-256: `71838c3ac0f19452cc9f00815f90f2675b24ae65935673669aa82f9f4e814c0a`. The report has no P0/P1/P2 findings and four P3 notes: frozen-plan context clarification; status belongs in `plan-status.md`; linker warning follow-up at OX-284; and stronger raw provenance for source checks. Claude Code ER-05 round 2 returned literal `VERDICT: PASS` (report SHA-256 `e7c193c3658bc30ec6e0e6b4a88f5c187cdcdf6591dd530c23145af536572f2e`), with no P0/P1/P2 findings. Three non-blocking P3 notes remain: the production excerpt hash field does not explicitly say it excludes the provenance header, the A/B manifest omits the command that generated its diff, and the macOS linker warning remains assigned to OX-284 final C.

## Release boundary

This is a child of REL-OX-01 with `Version increment=N/A` and `Release write set=N/A`. OX-284 is the sole patch bump and release point.
