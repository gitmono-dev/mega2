# FIX-OX-16 evidence

State: the card-specific A/B test, formatting, clippy, `cargo build` and `cargo build --tests` gates pass. The baseline failure's root cause is established as test-side (the test's fixed barrier is shorter than the publication-enabled fixture's per-request route-verification latency); the production revocation behavior itself is correct on both fixture modes. The accepted change is test-only.

## Scope and provenance

Base: `642c46987af3510e88a011bb1ecc908034bd51aa`. `A-source.rs` matches the base blob for `src/api/router/snapshot_raw_blob_tests.rs`; `B-final-source.rs` matches the current working file. The diff has exactly two test-only hunks inside `empty_raw_post_await_revocation_fails_before_another_eof_poll_or_empty_success`: the fixture switches from `Fixture::new()` to `Fixture::new_without_publication()`, and `fixture.counts.assert(1, 0)` runs immediately after the barrier so AC-1 is asserted directly instead of being taken from the diagnostic run. No production source file is modified; `libra diff -- src` shows only this test file.

## Root cause

The baseline failure was `entered.notified()` returning `Elapsed(())` at the fixed 10 s barrier (`A-VER-1`, exit 101, 33.39 s). One archived diagnostic run resolves it — `verification/diagnostic/diagnostic-run.stderr`, produced by `verification/diagnostic/diagnostic-source.rs` (the final source plus three temporary `zz_diagnostic_*` tests, removed afterwards):

1. The request is not blocked. Under the publication-enabled fixture the barrier does fire, at `entered_after_ms=12909`, with `whole=1 bytes=0` (`diagnostic-run.stderr:12`). `whole=1` proves the production path opened and polled the backend stream, so the zero-byte source really does reach the held EOF poll.
2. Publication-enabled raw-blob requests cost roughly 15 s each, not just the first: four sequential plain `GET blob?path=/empty` requests returned `200 OK` in 15,028 / 15,502 / 15,853 / 15,560 ms (`diagnostic-run.stderr:14-17`).
3. The same shape on the publication-disabled fixture costs 5 ms and 5 ms (`diagnostic-run.stderr:18-19`), so the latency lives in the rooted qualified-metadata route verification, not in the raw-blob path.
4. `pg_stat_activity` sampled while the request was inside the barrier showed no blocker and no lock wait; the single active session was executing the qualified-family catalog query (`WITH selected_namespaces ... pg_namespace`), i.e. CPU work rather than a held advisory lock (`diagnostic-run.stderr:9-11`).
5. The same diagnostic run also shows the production revocation result in publication-enabled mode: after the lease is released the request finishes `410 Gone`, `tail=0`, `drops=1`, both budgets 0 (`diagnostic-run.stderr:13`). The revocation contract therefore holds in both fixture modes.

Conclusion: the failure is the test's fixed 10 s barrier being shorter than the publication-enabled fixture's per-request route-verification latency. The production revocation behavior under test is correct, in both modes.

## Why the fixture changed instead of only lengthening the barrier

Lengthening the barrier alone would keep a witness whose runtime is dominated by an unrelated ~15 s per-request route verification. The same class of raw-blob case is not cheap at the tail either: in the FIX-OX-15 record the sibling `raw_blob::cold_raw_get_earns_source_proof_then_serves_one_current_stream_with_exact_headers` needed 877.83 s when it was re-run individually with `--test-threads=1` (`docs/plan/evidence/plan-20260920/cards/FIX-OX-15/verification/diagnostic/full-suite-tail/isolated-1.stdout:5`), so the barrier value cannot be made deterministic without weakening the test. The publication-disabled fixture still drives the real HTTP route, the real request/stream plumbing and the PostgreSQL-backed object store; it removes only the rooted-route verification cost that this card does not test. The publication-enabled revocation witness is preserved in the archived diagnostic run and is handed to `FIX-OX-18` as recorded below.

## Acceptance evidence

- AC-1: the zero-byte source enters the held EOF poll. B asserts `fixture.counts.assert(1, 0)` immediately after the barrier (one backend whole-object stream opened, zero delivered bytes); the archived diagnostic run additionally records `entered=true ... whole=1` in publication-enabled mode.
- AC-2: the lease is revoked during the hold (`release_lease` asserts `200`), and the completed response is non-retryable `410 LEASE_EXPIRED` (`SnapshotErrorCode::LeaseExpired` → HTTP 410, `retryable=false`).
- AC-3: after recovery the source is not polled again (`tail_polls == 0`) and the producer watchdog drops exactly once (`drops == 1`).
- AC-4: `response_budget.used() == 0` and `scratch_budget.used() == 0`, no receipt write is attempted, and a `410` status means no empty success.

## Verification

- A baseline VER-1: exit 101; `entered.notified()` expired at the 10 s barrier (33.39 s total).
- B final VER-1: exit 0; `1 passed; 0 failed; 2463 filtered out` (19.42 s).
- `cargo +nightly fmt --all --check`: exit 0.
- `cargo clippy --all-targets --all-features -- -D warnings`: exit 0.
- `cargo build` and `cargo build --tests`: exit 0; both emit the pre-existing macOS linker `__eh_frame section too large` warning, assigned to OX-284 final C.
- `source .env.test && cargo test --all`: not run for this card; the repository-wide gate is owned by OX-284 final C (same separation the FIX-OX-15 record used and the round-1 review accepted).
- Post-review regression (after the round-2 PASS, recorded before push): `source .env.test && RUST_LOG=error cargo test -p mega2 --lib empty_raw -- --test-threads=1` → exit 0, `3 passed; 0 failed; 2461 filtered out` (85.57 s). It covers this witness, `empty_raw_source_requires_physical_zero_exact_eof_and_current_empty_digest`, and `ceres::snapshot::rooted_metadata_projection::tests::streamed_cold_fact_keeps_large_memory_source_and_empty_raw_costs_exact`; evidence in `verification/regression-empty-raw.{exit,stdout,stderr}`.

## Durable handoff to FIX-OX-18

Recorded here and in the live ledger `docs/plan/plan-status.md` (section "FIX-OX-16 当前卡审计记录"):

- Raw-blob witness needing a publication-enabled rerun: `empty_raw_post_await_revocation_fails_before_another_eof_poll_or_empty_success` (this card). That rerun must record the entered latency against the barrier value and the resulting `410 LEASE_EXPIRED`/counter outcome on the publication-enabled path.
- Not added to this handoff: `raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll` is FIX-OX-17's own witness, already uses the publication-enabled fixture, and awaits `oneshot` before its barrier, so it is covered by FIX-OX-17's own card gate rather than by a publication-enabled rerun here.
- FIX-OX-18's implementation write set must be amended to add `src/api/router/snapshot_raw_blob_tests.rs`. The amendment lands under ER-03 before FIX-OX-18 starts; FIX-OX-18's card owns `docs/plan/plan-20260920.md`, so that card makes the amendment itself. If the raw-blob publication-enabled rerun turns out to be a separate behaviour axis from FIX-OX-18's rooted-HTTP barrier axis, it needs its own named `FIX-*` card instead of a write-set addition.
- Before treating the ~15 s as a production characteristic, FIX-OX-18 must check whether the cost is environmental: the only active session in the `pg_stat_activity` sample is a `pg_namespace`/catalog query whose cost can grow with the number of schemas present in the shared test database. Until that check lands, the measurement is a candidate production characteristic, not a confirmed one.

## Review record

- Round 1 (Claude Code, read-only): literal `VERDICT: FAIL`, one blocking P1 and one P2/P3 set. P1-1: the README quoted diagnostic figures and sub-paths that were not in the archived run. P2-1: the FIX-OX-18 handoff was not durable and the fixture-switch rationale was missing. P3-1..P3-5: AC-1 leant on a proxy, the redaction log disagreed with the sanitized bytes, the worktree snapshot predated the evidence set, the latency might be environmental, and the linker warning remains assigned to OX-284.
- Round 2 (Claude Code, read-only): literal `VERDICT: PASS` (report SHA-256 `4531e2b8c82821fb057011bff8ca272f83e325d9cd68f7fd3db519e75c864dca`; snapshot manifest SHA-256 `a147fed2604c13a29921c1358285bfd9da6f4840dfcb43b22c45ccf46ad9170e`). Round 1's blocking P1-1 is closed and the P2/P3 items are recorded above.
- Round-1 resolutions: P1-1 — this README now quotes only the archived run and cites `verification/diagnostic/diagnostic-run.{stdout,stderr}`; the referenced sub-directories were removed. P2-1 — the handoff above is recorded durably in `plan-status.md`, the fixture-switch rationale is stated above, and the FIX-OX-18 write-set amendment requirement is explicit. P3-1 — B now asserts the counters at the barrier. P3-2 — `redactions.json` records the reconstructed pre-sanitization originals. P3-3 — `verification/source-restoration/worktree-status.stdout` was recaptured after the evidence set was final. P3-4 — the environmental question is assigned to FIX-OX-18. P3-5 — the linker warning stays assigned to OX-284 final C.
- Round 1 measured the diagnostic on source `9053be7f9207739d36ae246085e912dde374fa02f1d758e939b03123c1d7e999` (12,813 ms barrier; 15,743 / 15,130 / 14,699 / 14,825 ms publication-enabled; 2 / 2 ms publication-disabled). Round 2 re-ran the same diagnostic against the reviewed source `700d966aabdb83deb0ea484bb995d92bf8bb483fb17b6743a9610be936c89e9f`, so the archived figures in this README are the round-2 values and `review/round-1/claude.stdout` quotes the earlier ones.
