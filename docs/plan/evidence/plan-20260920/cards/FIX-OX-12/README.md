# FIX-OX-12 evidence

State: A/B and formatting gates passed; Claude Code ER-05 round 2 returned literal `VERDICT: PASS`. The card remains `locally-accepted` until OX-284 supplies inherited final-tree C/D coverage. No version bump, push, tag, or release occurs before OX-284.

## Scope

Only the fixture lease passed by the two raw-stream tests in `src/api/router/snapshot_raw_blob_tests.rs` is in scope. The A source snapshot is the unmodified file at the task's starting HEAD; B sets a 3600-second lease for both rooted fixtures. The complete body, digest/header, receipt, corruption, and no-rebuild assertions remain in place.

## Verification

The card requires focused A/B runs for VER-1 and VER-2, preserving exact source snapshots, commands, exit codes, and sanitized logs under `verification/`.

## A baseline

- A source SHA-256: `7def8e48e8d631bffd1466b55b9e64f44dcc00d43e64109588984db690f10f66`.
- VER-1 exited 101 after 617.14s. The cold raw response headers, body, and source-read assertions passed; `LeaseExpired` then occurred at the alias response body `.unwrap()` (`A-source.rs:143`), so alias body verification and receipt-reuse assertions did not complete. Stdout SHA-256: `756ac25914f28d7907f4022665cc11fa8da6b4690e1c28622df7eca9516de9be`.
- VER-2 exited 101 after 616.31s. Missing/forged receipt assertions and the stored-corruption stream checks passed; `LeaseExpired` then occurred while reading the restored raw body (`A-source.rs:235`). Stdout SHA-256: `09d6faa5b7431b85888336a8681f3ebbbde92b30829fb16c58003d40f958bbbf`.

## B candidate

- B source SHA-256: `600d9ae359dd7315fc68ae671c9ff460ac84d3e6798a12e89f7bafd3e1711072`; the A/B diff SHA-256 is `5bf08a694c082c1300bae2048fc5be36319469002026c245da540438ed937fcb`. It changes only the two fixture constructors to use `lease_seconds: Some(3600)`.
- VER-1 passed: `1 passed; 0 failed; 2463 filtered out; finished in 884.03s`. Stdout SHA-256: `b15c002bf49f1f153c218ef812e04dbe182256d2e8f9e1df876a67416c52f4bc`.
- VER-2 passed: `1 passed; 0 failed; 2463 filtered out; finished in 920.25s`. This exercised missing and forged receipts, the stored-corruption stream failure, and restored full-body verification. Stdout SHA-256: `c49036d421c9baf5c24bb77052a107c7ae3bddbc0dd5f12d32c79597dd0a7d92`.
- All four Cargo runs emitted the same macOS linker `__eh_frame section too large` warning; it is retained in stderr for the OX-284 final build gate to evaluate. B-VER-1 compiler stderr had one local checkout path redacted under ER-11; hashes are recorded in `redactions.json`.
- `cargo +nightly fmt --all --check` exited 0; raw stdout, stderr and exit files are retained under `verification/FMT.*` with hashes in `verification-results.json`.
- `test_harness_seconds` records Cargo's `finished in` duration, excluding any compilation time shown in stderr.

## ER-05 review

- Round 1 report SHA-256 `22e3da46e45aeecc09a4f90afd93299b5a84ba7172597178d9eaa25e6ac846a4`: literal `VERDICT: FAIL`. Its P1 failure-location corrections and P2 fmt raw-result request are addressed in the round-2 packet; the P3 duration and state wording were also corrected.
- Round 2 report SHA-256 `272075b801146dc0b96d584b7a5de4ad79888baac4cde8c1d0763b307dd57fda`: literal `VERDICT: PASS`, exit 0. The review confirmed both A panic locations, both B passes, preserved assertions, formatting evidence, ER-11 redaction, and the OX-284 release boundary.
- The stored round-2 prompt replaces its two absolute checkout paths with repository-relative paths under ER-11. `review-metadata.json` preserves the SHA-256 of the prompt Claude reviewed and records the sanitized stored prompt hash; `redactions.json` records both hashes.
- The round-2 review request had one non-blocking wording issue: “every Cargo run” meant the four focused test commands; `FMT.stderr` is correctly empty. The immutable request and report remain preserved as audit history.
- Prompt, snapshot manifest, report hashes, verdicts, and non-blocking findings are recorded in each round's `review-metadata.json`.
