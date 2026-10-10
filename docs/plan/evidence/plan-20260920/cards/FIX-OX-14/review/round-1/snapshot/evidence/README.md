# FIX-OX-14 evidence

State: the focused A/B test, `cargo +nightly fmt --all --check`, and `cargo clippy --all-targets --all-features -- -D warnings` have passed. A changes only the target `retryable` expectation from `true` to `false` and fails with actual `true`; B retains `true` and passes. Claude Code ER-05 review is pending. This card inherits final C/D from OX-284; no version bump, push, tag, or release is permitted before OX-284.

## Scope and source history

The owned test hunk is in `src/api/router/snapshot_rooted_metadata_tests.rs`; it asserts that GET for a non-current verified file fact returns 503 `METADATA_NOT_READY` with `retryable=true`, while preserving the independent HEAD `/empty` 200/zero-content-size assertion and zero body reads. Production error mapping already marks `MetadataNotReady` retryable in `src/api/router/snapshot_router.rs`. The B source is already present at base commit `ac7acfdba08a427bc295be71ec5f2f465a515e32` (and in the earlier checkpoint); this card performs its own A/B acceptance and review for the checkpoint-owned hunk. No production behavior changed for this card.

## A/B results

- A: exact B source except the target expectation is reversed to `false`; focused test exited 101 because actual retryable was `true` while expected was `false` (harness 32.27s).
- B: current source with expectation `true`; focused test exited 0 (`1 passed; 0 failed; 2463 filtered out`, harness 34.18s).
- `cargo +nightly fmt --all --check`: exit 0.
- `cargo clippy --all-targets --all-features -- -D warnings`: exit 0; `verification-results.json` binds it to the exact B source SHA and base commit.
- A/B source diff contains one test-only hunk. `base-source-check.json` confirms B matches the recorded base source.

The A/B Cargo stderr and clippy stderr contained one absolute execution-checkout path each; `redactions.json` records original and sanitized hashes. The macOS linker emitted `__eh_frame section too large` during test linking; carry it to OX-284's final C build gate for disposition.

## Release boundary

This card is a child of REL-OX-01 with `Version increment=N/A` and `Release write set=N/A`. OX-284 is the sole patch bump and release point.
