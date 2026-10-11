# FIX-OX-17 evidence

State: the card's focused witness passes on the final tree, and `fmt` / `clippy` / `cargo build` / `cargo build --tests` are green. The recorded FIX-OX-04 baseline (exit 101 at the `entered` barrier) did **not** reproduce in isolation on the current tree; the timing diagnostics show the failure was a barrier-margin failure, and the accepted change restores margin without changing the witness or its assertions.

## Scope and provenance

Base: `f60d771c79a4483df958ec9c278e3c7a656c2da5` (the FIX-OX-16 commit). `A-source.rs` matches the base blob for `src/api/router/snapshot_raw_blob_tests.rs`; `B-final-source.rs` matches the current working file. The diff has exactly one test-only hunk, in `raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll`: the `entered` barrier goes from 10 s to 60 s and the post-release window from 10 s to 30 s. No assertion changes and no production source file is modified; `libra diff -- src` shows only this test file.

## Root cause

The record for this witness is a focused exit 101 with `entered.notified()` returning `Elapsed(())`, and it appears again in the FIX-OX-15 tree-wide failure list. Two archived diagnostics resolve what actually happens — both are reproduced inline in this packet and stored under `verification/diagnostic/`.

1. **The witness is correct and does reach the held poll.** The timing diagnostic (`diagnostic-run.stderr`) runs all four cases of the witness unchanged against the base source and records `entered=true` for every case, `status=200`, and a body that terminates with `LEASE_EXPIRED` after the lease is revoked, with `tail=0`, `drops=1`, both budgets 0, `eof=true`, and the case-correct delivered prefix (0 bytes for the 1-byte-prefix cases, 1,048,576 bytes for the whole-prefix cases).
2. **The barrier is the fragile part.** The same run measures how much of the 10 s budget the barrier actually consumes: `barrier_wait_ms` = 6,200 / 6,141 / 9,448 / 8,817 for cases 0..3. Cases 2 and 3 also pay an extra `revalidate_access` cycle because the 1 MiB prefix fills the chunk and `require_eof` polls again. That leaves 6–38 % headroom, which is why the witness passes in isolation but fails under load.
3. **The recorded failure mode is reproducible.** The margin probe (`barrier-margin-run.stderr`) repeats cases 0 and 2 with a 5 s barrier and a 60 s follow-up wait: `entered_within_5s=false` and `entered_within_60s=true` for both. A barrier shorter than the publication-enabled body-path route verification therefore produces exactly the recorded `Elapsed(())` exit 101, and a longer barrier reaches the held poll.
4. **The baseline does not reproduce at 10 s.** Running the unmodified base source through VER-1 exits 0 in 231.48 s. The change is therefore a margin/robustness fix for a load-dependent failure, not a repair of a wrong assertion.

The underlying cost is the publication-enabled rooted route verification (about 9–10 s for `map` and for the request itself, plus up to 9.4 s inside the barrier). That cost is a candidate production characteristic owned by the pre-existing card `FIX-OX-18`, which must also decide whether it is environmental; this card neither waives nor resolves it.

## Why the barrier was raised instead of switching fixtures

FIX-OX-16 moved its witness to the publication-disabled fixture because its 13 s baseline exceeded the barrier on every run. Here the publication-enabled witness passes and exercises the publication-enabled body path, so removing that coverage would be a loss for no gain: raising the barrier keeps the same fixture, the same assertions and the same code path, and only removes a timing budget that was 6–38 % from expiry. The FIX-OX-16 ledger already records that this witness is covered by FIX-OX-17's own gate and is not part of the FIX-OX-16 publication-enabled handoff, which stays accurate.

## Acceptance evidence

- AC-1: all four cases revoke only after the backend is actually held. The witness asserts `fixture.counts.assert(1, prefix_length)` immediately after the barrier, and both diagnostics record `whole=1` with the case-correct `bytes` value (`1` or `1,048,689`) at the barrier.
- AC-2: after recovery the body terminates with `LEASE_EXPIRED` (`410`, non-retryable) and the delivered prefix matches the case — 0 bytes for cases 0/1, 1,048,576 bytes for cases 2/3 (`diagnostic-run.stderr`).
- AC-3: the tail is not polled again (`tail_polls == 0`) and the producer drops exactly once (`drops == 1`).
- AC-4: `response_budget.used() == 0` and `scratch_budget.used() == 0` after every case, and the stream ends (`eof=true`).

## Verification

- A VER-1 (base source, 10 s barriers): exit 0; `1 passed; 0 failed; 2463 filtered out` (231.48 s). Recorded honestly: the historical failure did not reproduce.
- B final VER-1 (60 s / 30 s windows): exit 0; `1 passed; 0 failed; 2463 filtered out` (210.01 s).
- `cargo +nightly fmt --all --check`: exit 0.
- `cargo clippy --all-targets --all-features -- -D warnings`: exit 0.
- `cargo build` and `cargo build --tests`: exit 0; both emit the pre-existing macOS linker `__eh_frame section too large` warning, assigned to OX-284 final C.
- `source .env.test && cargo test --all`: not run for this card; the repository-wide gate is owned by OX-284 final C.

## Follow-ups (non-blocking for this card)

- `FIX-OX-18` owns the publication-enabled route-verification cost measured here (about 9–10 s per request in isolation, up to 9.4 s inside the body path) and must decide whether it is environmental before it is called a production characteristic.
- The body path calls `revalidate_access` before and after each backend poll; the extra cycle visible in cases 2/3 is the reason their barrier consumption is 2.6–3.3 s higher. That observation is recorded for FIX-OX-18 and is not changed by this card.

## Review record

- Round 1 (Claude Code, read-only): pending.
