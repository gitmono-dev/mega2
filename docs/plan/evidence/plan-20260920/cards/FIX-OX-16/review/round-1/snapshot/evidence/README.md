# FIX-OX-16 evidence

State: the card-specific A/B test, formatting, and clippy gates pass. Root cause of the recorded baseline failure is established as test-side (barrier duration versus fixture mode); the production revocation behavior itself is correct. The card's accepted fix is test-only.

## Scope and provenance

The base is `642c46987af3510e88a011bb1ecc908034bd51aa`. `A-source.rs` matches the base blob for `src/api/router/snapshot_raw_blob_tests.rs`; `B-final-source.rs` matches the current working file. The unified diff has exactly one test-only hunk: the focused test's fixture changes from `Fixture::new()` (publication enabled, rooted qualified-metadata routes) to `Fixture::new_without_publication()`. No production source file is modified; `libra diff -- src` shows only this test file.

## Root-cause investigation

The recorded baseline failure was `entered.notified()` timing out at 10 s, with no deterministic proof that the zero-byte source ever reached the held backend EOF poll. Temporary diagnostics (removed before the final source; the diagnostic snapshot is preserved under `verification/diagnostic/`) established:

1. The request is not permanently blocked. Under the publication-enabled fixture the barrier does fire, but only after 13,123 ms: `entered=true whole=1 bytes=0` (`verification/diagnostic/entered-latency/`). `whole=1` proves the production path opened and polled the backend stream, so the held EOF poll is reached.
2. The publication-enabled raw-blob request costs roughly 15 s on every call, not just the first: four sequential plain `GET blob?path=/empty` requests returned `200 OK` in 14,353 / 15,909 / 15,114 / 15,986 ms (`verification/diagnostic/plain-get-timing/`).
3. The same four-request shape on the publication-disabled fixture costs 6 ms and 4 ms (`verification/diagnostic/plain-get-timing-without-publication/`), so the latency is entirely in the rooted qualified-metadata route/verification path, not in the raw-blob path.
4. `pg_stat_activity` sampled while the request was inside the 10 s barrier showed no lock waiter and no blocker for the request session, so the delay is not a held advisory lock (`verification/diagnostic/pg-stat-activity/`).

Conclusion: the failure is a test-side barrier that is shorter than the publication-enabled fixture's per-request route-verification latency. The production revocation behavior under test is correct. The publication-enabled route-admission latency is a production characteristic already owned by the pre-existing named card `FIX-OX-18`; this card records the measurement and hands the publication-enabled rerun of the raw-blob revocation witnesses to `FIX-OX-18`'s execution ledger, without presuming a test-hook or production defect.

## Acceptance evidence

- AC-1: the zero-byte source enters the held EOF poll (`entered.notified()` returns inside the card barrier; the diagnostic run records `whole=1` at the barrier).
- AC-2: the lease is revoked during the hold, and the completed response is non-retryable `410 LEASE_EXPIRED`.
- AC-3: after recovery the source is not polled again (`tail_polls == 0`) and the producer watchdog drops exactly once (`drops == 1`).
- AC-4: `response_budget.used() == 0` and `scratch_budget.used() == 0`; no empty success is returned, and no receipt write is attempted.

## Verification

- A baseline VER-1: exit 101; `entered.notified()` elapsed at the 10 s barrier.
- B final VER-1: exit 0; `1 passed; 0 failed; 2463 filtered out` (19.22 s).
- `cargo +nightly fmt --all --check`: exit 0.
- `cargo clippy --all-targets --all-features -- -D warnings`: exit 0.
- `cargo build` and `cargo build --tests`: exit 0; both emit the macOS linker `__eh_frame section too large` warning, assigned to OX-284 final C.

## Follow-ups (non-blocking for this card)

- `FIX-OX-18` must include a publication-enabled rerun of the raw-blob revocation witnesses and capture the route-admission behavior; the printed `15 s` per-request route-verification latency is recorded here as its input evidence.
- `FIX-OX-17` owns the sibling fragmented/EOF revocation witness in the same file and has the same fixture-mode and barrier question.
- OX-284 final C still owns the repository-wide `cargo test --all` gate.
