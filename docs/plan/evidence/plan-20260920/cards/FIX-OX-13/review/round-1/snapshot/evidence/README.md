# FIX-OX-13 evidence

State: all three focused A/B gates and `cargo +nightly fmt --all --check` passed; Claude Code ER-05 review is pending. This card remains locally accepted only after a literal Claude `VERDICT: PASS`; final C/D coverage is inherited from OX-284. No version bump, push, tag, or release occurs before OX-284.

## Scope

The change is limited to three test SQL projections in `src/jupiter/storage/qualified_metadata_gc_tests.rs` and `src/jupiter/storage/qualified_metadata_source_revision_tests.rs`. PostgreSQL `CASE` expressions map `true` to `1::bigint` and `false` to `0::bigint`. Assertions, production SQL, and schemas are unchanged.

## A baseline

The unchanged source at base commit `7c15d36f287060756610b56adb1b893dd0d57ca7` was tested with each focused command from FIX-OX-13 VER-1..3. All three exited 101 with PostgreSQL SQLSTATE `42846` (`cannot cast type boolean to bigint`). Their harness times were 10.94s, 9.90s, and 10.00s. Exact source copies, commands, exits, and sanitized logs are recorded under `verification/` and in `verification-results.json`.

## B candidate

- VER-1 passed: `1 passed; 0 failed; 2463 filtered out`; harness time 18.44s.
- VER-2 passed: `1 passed; 0 failed; 2463 filtered out`; harness time 10.63s.
- VER-3 passed: `1 passed; 0 failed; 2463 filtered out`; harness time 10.18s.
- `cargo +nightly fmt --all --check` exited 0. Raw stdout, stderr, and exit files are retained.
- The six Cargo test stderr files contain the macOS linker warning `__eh_frame section too large`; OX-284's final build gate must evaluate it. B-VER-1's one absolute checkout path was redacted under ER-11; `redactions.json` records original and sanitized hashes.

The A/B manifest records both source hashes, the three diff hunks, and the exact focused test names. `test_harness_seconds` excludes compilation time, which remains in stderr.

## ER-05 review

Claude Code's independent read-only review is pending. Its snapshot will contain the task card, applicable template clauses, A/B source copies, and verification evidence. The card must not be marked locally accepted or committed until the review returns literal `VERDICT: PASS`.

## Release boundary

This is a child of REL-OX-01 with `Version increment=N/A` and `Release write set=N/A`. OX-284 is the sole patch release point.
