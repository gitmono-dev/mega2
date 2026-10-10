# FIX-OX-12 ER-05 review (round on the snapshot packet)

I didn't modify any files or run any tests. I worked only from `docs/plan/evidence/plan-20260920/cards/FIX-OX-12/review/snapshot/`; the `../snapshot/` path from the request doesn't exist relative to the checkout. I ignored the `round-1/` directory next to the snapshot. I also had no shell in this session, so I could not recompute SHA-256 hashes myself (see Q3).

**Result: the code change is correct, but the packet records the wrong failure points for both A runs. That is one blocking evidence-integrity finding.**

## Q1 — 3600s lease and otherwise-default configuration: supported

- **Default path:** `Fixture::new()` → `new_with_pg_config(false)` → … → `new_in_publication_mode(false, 0, &[], false, true)` → `new_in_publication_mode_with_options(false, 0, &[], false, true, FixtureOptions::default())` (`code/fixture-options-scaffold.rs:641-701`).
- **B path:** `new_rooted_with_options(&[], opts)` → `new_in_publication_mode_with_options(false, 0, objects, false, true, options)` (`:704-709`).
- B passes `FixtureOptions { lease_seconds: Some(3600), ..Default::default() }`. So the only difference from the default fixture is the lease.
- The lease goes into the resolve request body before `POST /api/v2/snapshots/resolve` (`:965-978`). That means it is in place before the rooted resolve.

## Q2 — Diff scope and kept assertions: supported

- `evidence/verification/A-B-source.diff` has exactly 2 hunks. Each replaces `Fixture::new().await` with the constructor above.
- The size change confirms nothing else moved: each hunk removes 40 bytes and adds 191, so +151 × 2 = 302. That matches the file sizes in `snapshot-manifest.json` (29299 → 29601).
- I compared A lines 101–149 with B 101–156, and A 192–240 with B 199–254. The bodies are identical. All the assertions you listed are still there:
  - full body and headers (content-length, x-mega-content-size, etag/digest, fs-kind, vary, cache-control)
  - source-read counts and `counts.assert(2, 2*len)`
  - alias reuse: zero bytes read, `receipt_writes=0`, `receipt_reads=1`
  - missing and forged receipts: 502 `INTEGRITY_ERROR` with `assert(0,0)`
  - stored corruption: the stream errors with whole=1, range=0, no receipt writes
  - restored full body
- `code/candidate-source.rs` and `B-source.rs` have the same recorded hash, so the reviewed candidate is the B source.

## Q3 — A/B runs: B is fully supported; A fails as needed, but at different points than recorded

**B runs:**
- Exit codes are `0` in both `B-VER-1.exit` and `B-VER-2.exit`.
- The stdout logs show `1 passed; 0 failed; 2463 filtered out`, finishing in 884.03s and 920.25s.
- The commands match VER-1 and VER-2 on the task card.
- Neither stderr contains an error.

**A runs:**
- Exit codes are `101` in both files, with `LeaseExpired` panics at 617.14s and 616.31s.

**Hashes:** The A/B source, diff and log hashes agree across `verification-results.json`, `ab-manifest.json`, `snapshot-manifest.json`, `README.md` and `task-card.md`. The exit-file sizes (4 bytes for `101\n`, 2 bytes for `0\n`) and their shared hashes are consistent. I did not recompute any SHA-256 values.

- **P1 (blocking) — the recorded A failure points contradict the raw logs.**
  - **A-VER-1:** it panics at `snapshot_raw_blob_tests.rs:143:14` (`A-VER-1.stdout:9`). In `code/A-source.rs`, line 143 is the `.unwrap()` on the `/alias` body read. The cold-path body and header assertions at lines 103–135 had already passed. The packet says it failed "before the complete cold raw body assertions":
    - `verification-results.json` `test_result`
    - `task-card.md:6`
    - `README.md:16`
  - **A-VER-2:** it panics at `:235:14` (`A-VER-2.stdout:9`). That is the `.unwrap()` on the final restored-body read. The missing and forged receipt checks (lines 202–211) and the stored-corruption checks (221–228) ran and passed in A. The packet says it failed "while sending the first forged/missing-receipt request … before the intended receipt assertions":
    - `README.md:17`
    - `verification-results.json`
    - `task-card.md:6`
  - Both A runs are still valid baselines. Each fails with `LeaseExpired` before its remaining targeted assertions (alias receipt reuse, restored full body), so the fix is justified. But the permanent record misstates which assertions the baseline blocked.
  - **Fix:** correct those three statements to the actual panic sites, refresh the affected hashes and manifests, and re-review. No code or rerun is needed.
- **P3 —** The recorded `duration_seconds` is the test harness's "finished in" time, not the full command time including compilation. It's fine, but should be labelled.

## Q4 — fmt, redaction, exposure: supported, with one gap

- `cargo +nightly fmt --all --check` exit=0 appears only in `verification-results.json` and the narrative. The packet has no fmt exit or log file.
  - **P2:** add a raw exit file and output for fmt alongside the other runs.
- `redactions.json` records the ER-11 policy, file, reason, replacement token, occurrence count of 1, and the original and sanitized SHA-256. The sanitized hash matches the `B-VER-1.stderr` hash used everywhere else, and the file shows `[REDACTED_EXECUTION_PATH]` once.
  - Anyone holding the original log can reproduce the redaction. The original path is not disclosed, which is correct.
- I searched the snapshot for absolute paths (`/Users`, `/Volumes`, `/home`) and for keys and credentials. The only matches were the test-constant reference `Bearer {TOKEN}`. The diff headers use repo-relative paths.

## Q5 — Release boundary: supported

- The card has `Version increment=N/A` and `Release write set=N/A`, and names its boundary as a "plan release child of REL-OX-01", with C/D coverage from OX-284 and D-OX-TAG.
- Lifecycle is `in-progress` with Acceptance empty.
- `README.md:3` and the card both say no bump, push, tag or Release happens before OX-284.
- The status row says "未 Push、未 bump、未发布" (not pushed, not bumped, not released), and the card says this is card-level VER only, not the final-tree C.
- Nothing in the packet claims final C/D, a Docker release, or a version change.

## Other non-blocking

- **P3:** `README.md:3` still says "State: pending A/B execution", and `:7` says "B will add". Both are stale now that A/B evidence is recorded.
- **Note:** every run has the linker `__eh_frame` warning (`warning: linker stderr`). It comes from the environment and is outside this card's scope. OX-284's final-tree `cargo build --tests` zero-warning gate should account for it.

VERDICT: FAIL
