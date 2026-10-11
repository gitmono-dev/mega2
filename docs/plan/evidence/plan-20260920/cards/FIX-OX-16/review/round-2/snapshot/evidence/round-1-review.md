# FIX-OX-16 ER-05 review, round 1

**Result: FAIL.** There is one blocking P1: the README's root-cause section quotes measurements and artifact paths that are not in the archived diagnostic evidence. The underlying attribution is still supported by the archived run. The code change, the A/B pair, the AC coverage and the gates are all acceptable.

## Q1 — Root cause

The archived run does support the attribution. In `evidence/diagnostic/diagnostic-run.stderr:9-19`:
- With publication enabled, the barrier fires after **12,813 ms**, beyond the 10 s timeout. At that point `whole=1 bytes=0`.
- After the lease is released, the request finishes with **410**, `tail=0`, `drops=1`, and both budgets at 0. So the revocation behaves correctly even in publication-enabled mode.
- Plain publication-enabled GETs take **15743 / 15130 / 14699 / 14825 ms**. With publication disabled they take **2 / 2 ms**.
- The `pg_stat_activity` sample shows no blockers. The one active session is running a catalog query (`selected_namespaces … pg_namespace`), not waiting on a lock.

This also explains why only this test failed. The sibling tests await `oneshot` before their barrier starts. This test spawns the whole request inside the barrier window (B `:578-581`), so the barrier also has to absorb the route latency.

However, `evidence/README.md` does not describe this archived run (see P1-1).

## Q2 — A/B validity: confirmed

- A and B differ by exactly one test-only hunk at line 556 (`Fixture::new()` → `Fixture::new_without_publication()`). This is shown by `A-B-source.diff`, the diff exit code of 1, and `tracked-source-diff.stdout`.
- A exits 101 at `snapshot_raw_blob_tests.rs:581:14`. That line is the `.unwrap()` of `timeout(10s, entered.notified())`, i.e. the barrier itself, not the post-release timeout.
- B exits 0 with `1 passed; 0 failed`.
- The recorded hashes are consistent with each other across `ab-manifest.json`, `source-restoration-check.json` and `verification-results.json`. I could not recompute them in this read-only review.

## Q3 — Acceptance criteria

The B test (`B-snapshot_raw_blob_tests.rs:553-597`) asserts all four ACs for both the `Some(empty)` and `None` fragment variants:

| AC | Assertion in B |
|---|---|
| AC-1 | Barrier unwrap at `:579-581` |
| AC-2 | `release_lease` asserts 200, then `error(…, 410, "LEASE_EXPIRED", false)` at `:590` |
| AC-3 | `tail_polls == 0` and `drops == 1` |
| AC-4 | Both budgets at 0, `receipt_writes == 0`, and a 410 status means no empty success |

The raw exits support this: B exits 0.

## Q4 — Scope honesty

- **Production source:** `libra diff -- src` shows only the test file.
- **Diagnostic instrumentation:** the `zz_diagnostic_*` functions are absent from B, which is byte-identical to A apart from the hunk.
- **FIX-OX-18 handoff:** the handoff to FIX-OX-18 exists only in the README (see P2-1).

## Q5 — Gates (raw exits)

| Gate | Exit |
|---|---|
| `cargo +nightly fmt --all --check` | 0 (empty output) |
| `cargo clippy --all-targets --all-features -- -D warnings` | 0 |
| `cargo build` | 0, with the macOS `__eh_frame` linker warning |
| `cargo build --tests` | 0, with the macOS `__eh_frame` linker warning |
| `source .env.test && cargo test --all` | not run |

The decision not to run the full suite is stated plainly in `verification-results.json` (`not_run_for_this_card`, owned by OX-284 final C). That matches the FIX-OX-15 separation and is honest.

## Q6 — No release actions: confirmed

- Builds still report `mega2 v0.42.25`, so there is no version bump.
- `worktree-status.stdout` shows `## main...origin/main` with no ahead count. Only the test file is modified, so nothing has been committed or pushed.
- Neither a tag nor a GitHub Release is referenced anywhere.

## Findings

### P1 (blocking)

**P1-1 — The README cites diagnostic measurements and paths that are not in the archived evidence.** At `evidence/README.md:13-16`:
- The barrier time is given as **13,123 ms**; the archive says **12,813**.
- The plain GETs are given as **14,353 / 15,909 / 15,114 / 15,986 ms**; the archive says **15743 / 15130 / 14699 / 14825**.
- The publication-disabled timings are given as **6 / 4 ms**; the archive says **2 / 2**.
- It points to `verification/diagnostic/entered-latency/`, `plain-get-timing/`, `plain-get-timing-without-publication/` and `pg-stat-activity/`. None of these exist. `ab-manifest.json` lists only the four `diagnostic-run.*` / `diagnostic-source.rs` artifacts.

`verification-results.json` agrees with the archive, so the evidence package contradicts itself. Q1 specifically asks for this cross-check.

**Required fix:** either rewrite README items 1–4 to quote and cite only `diagnostic/diagnostic-run.{stdout,stderr}`, or archive the run that produced the quoted figures and list it in the manifest.

### P2 (non-blocking; must be resolved in the card's evidence/status commit)

**P2-1 — The handoff to FIX-OX-18 is not durable, and the committed test loses publication-enabled coverage.**
- The fix moves this witness off the publication-enabled fixture, which was `Fixture::new()`, the default. The publication-enabled revocation path was verified only once, in a diagnostic test that has since been removed.
- `context/follow-up-card-FIX-OX-18.md` covers actual-Q and rooted-reader barriers. Its write set does not include `snapshot_raw_blob_tests.rs`, and it says nothing about raw-blob latency. The README's "already owned by FIX-OX-18" therefore overstates the current ownership.
- `plan-file-diff` is empty, even though the card's write set lists `docs/plan/plan-20260920.md`.

**Required:** record the handoff in plan-status and the FIX-OX-18 ledger the same way the FIX-OX-15 P2 was recorded. That record must say which raw-blob witnesses need a publication-enabled rerun, and it must say whether FIX-OX-18's write set needs an amendment.

The README should also say why the fix switched fixtures instead of lengthening the barrier while keeping `Fixture::new()`.

### P3 (non-blocking)

- **P3-1 — AC-1 leans on a proxy from another mode.** The README supports AC-1 with `whole=1` from the publication-enabled diagnostic run. B never asserts the counters at the barrier. Adding `fixture.counts.assert(1, 0)` right after `:581`, as the sibling test does at its barrier, would make the witness direct.
- **P3-2 — The redaction log is inconsistent.** `B-final-VER-1.stderr`, `clippy-final.stderr` and `diagnostic-run.stderr` all contain `[REDACTED_EXECUTION_PATH]`. Yet `redactions.json` records no replacements for them, with identical original and sanitized hashes. Only `cargo-build-tests.stderr` logs a replacement.
- **P3-3 — The worktree snapshot is older than the final evidence set.** `worktree-status.stdout` was captured before `ab-manifest.json`, `verification-results.json`, `redactions.json`, `cargo-build*` and `source-restoration*` existed. Recapture it before the commit.
- **P3-4 — The latency cause may be environmental.** The only active session in the `pg_stat_activity` sample is a `pg_namespace` catalog query. Its cost may grow with the number of test schemas in the shared database. FIX-OX-18 should check that before calling the 15 s a production characteristic.
- **P3-5 — Linker warning.** The macOS `__eh_frame` warning technically breaks AGENTS.md's "0 warnings" rule for both builds. It is pre-existing, and assigning it to OX-284 final C is consistent with earlier cards.

## Summary

Closing P1-1 is the only change needed to pass. The P2-1 ledger and handoff record should land in the same commit.

VERDICT: FAIL
