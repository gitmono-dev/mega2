# FIX-OX-12 evidence

State: pending A/B execution and Claude Code ER-05 review. This is a plan release child of REL-OX-01; no version bump, push, tag, or release is allowed before OX-284.

## Scope

Only the fixture lease passed by the two raw-stream tests in `src/api/router/snapshot_raw_blob_tests.rs` is in scope. The A source snapshot is the unmodified file at the task's starting HEAD; B will add a 3600-second lease to both rooted fixtures. The complete body, digest/header, receipt, corruption, and no-rebuild assertions remain in place.

## Verification

The card requires focused A/B runs for VER-1 and VER-2, preserving exact source snapshots, commands, exit codes, and sanitized logs under `verification/`.

## A baseline

- A source SHA-256: `7def8e48e8d631bffd1466b55b9e64f44dcc00d43e64109588984db690f10f66`.
- VER-1 exited 101 after 617.14s. It failed at the original raw body read with `LeaseExpired`; the retained stdout SHA-256 is `756ac25914f28d7907f4022665cc11fa8da6b4690e1c28622df7eca9516de9be`.
- VER-2 exited 101 after 616.31s. It failed while sending the first forged/missing-receipt request with `LeaseExpired`, before the intended receipt assertions; the retained stdout SHA-256 is `09d6faa5b7431b85888336a8681f3ebbbde92b30829fb16c58003d40f958bbbf`.

## B candidate

- B source SHA-256: `600d9ae359dd7315fc68ae671c9ff460ac84d3e6798a12e89f7bafd3e1711072`; the A/B diff SHA-256 is `5bf08a694c082c1300bae2048fc5be36319469002026c245da540438ed937fcb`. It changes only the two fixture constructors to use `lease_seconds: Some(3600)`.
- VER-1 passed: `1 passed; 0 failed; 2463 filtered out; finished in 884.03s`. Stdout SHA-256: `b15c002bf49f1f153c218ef812e04dbe182256d2e8f9e1df876a67416c52f4bc`.
- VER-2 passed: `1 passed; 0 failed; 2463 filtered out; finished in 920.25s`. This exercised missing and forged receipts, the stored-corruption stream failure, and restored full-body verification. Stdout SHA-256: `c49036d421c9baf5c24bb77052a107c7ae3bddbc0dd5f12d32c79597dd0a7d92`.
- `cargo +nightly fmt --all --check` exited 0. B-VER-1 compiler stderr had one local checkout path redacted under ER-11; hashes are recorded in `redactions.json`.
