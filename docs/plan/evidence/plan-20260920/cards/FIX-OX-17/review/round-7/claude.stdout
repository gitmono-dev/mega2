# FIX-OX-17 ER-05 review, round 7

## Summary

The code change, the A/B mechanics and the gates are acceptable, as in rounds 3–6. Three of the round-6 items are closed:

- **Round-6 P1-1:** the README now cites the round-4 report SHA correctly.
- **Round-6 P2-1:** the ledger now carries the full round-5 report SHA.
- **Round-6 P3-1:** the README now has a round-5 entry.

The §四 parenthesis is also fixed.

**Verdict: FAIL.** The §一 trailing comma (round-6 P3-2) is still in the ledger. Two records say it was fixed:

- `verification-results.json` `review_history[5].resolutions`
- `round-6-review-metadata.json`, where P3-2/P3-3 is marked "fixed in round 7"

The comma itself is cosmetic. The blocking problem is that an authoritative record claims a correction that was not made. Round 6 made exactly this pattern its blocking P1-1, and I am applying the same standard here.

Everything left is text-only. No re-run is needed.

## Findings

### P1 (blocking)

**P1-1: The §一 trailing comma is still present, and two records say it was fixed.**

- **The defect.** In `context/plan-status.md` §一, the plan-20260920 row's last cell still ends `…`cargo build --tests` 均通过，ER-05 round-7 审阅中，`. It has a trailing "，" and no closing `|`. The next line is the `plan-20260921.md` row. This is the same text round 6 flagged, with only the round number changed.
- **The false claims.**
  - `evidence/verification-results.json` `review_history[5].resolutions` says "§一 trailing comma and §四 unmatched parenthesis fixed".
  - `evidence/round-6-review-metadata.json` marks the P3-2/P3-3 finding "fixed in round 7".
  - Only the §四 half of each claim is true.
- **Fix:**
  1. End the cell with "审阅中 |", or close the sentence properly.
  2. Re-hash `plan-status.md`.
  3. Leave the resolution claims as they are, since they become true.
  4. As a check, search for `审阅中，\n` and confirm every table row ends with `|`.

### P2 (non-blocking)

**P2-1: Earlier review_history and README entries credit fixes to rounds that did not complete them.**

- **`review_history[4]` (round 5)** says "the round-4 report SHA corrected to c20503e7… in the README and the ledger". Round 6 found the README part was not done; it was actually done in round 7.
- **The README "Round-5 resolutions" line** makes the same claim.
- **`review_history[3]` (round 4)** and **the README "Round-4 resolutions" line** still say the SHA phrasing was normalised "in all four locations" during round 4. Round 5 found three of the four were malformed.

Each correction now exists, so none of these claims an absent correction. The history is still inaccurate. Round 6 P3-1 suggested re-attributing these. Add a short qualifier to each, for example "(README half completed in round 7)" or "(three locations completed in round 5)".

### P3

- **P3-1: Unbalanced parenthesis in the README round-4 entry.** It reads "…(report SHA-256 `c20503e7b81fd243…`; snapshot manifest SHA-256 `9168b3f0…`); findings were text-only)." That is one "(" and two ")". The stray ")" sits in the line that round-6 P1-1 asked to fix.
- **P3-2: The README has no round-6 entry.** The README § Review record covers rounds 1–5. `verification-results.json` and the ledger both record round 6 (FAIL, report `28025dbe…`) and its resolutions. This is the same one-round lag as round-4 P3-3. The entries are also still out of order (2, 3, 4, 5, then 1).
- **P3-3: The ledger has no round-6 report SHA.** The FIX-OX-17 audit record's ER-05 bullet records round 6 without one. The value is `28025dbe92fe8e89…`, from `round-6-review-metadata.json` and `redactions.json` for `review/round-6/claude.stdout`.
- **P3-4: Historical manifest labels.** `evidence/round-5-snapshot-manifest.json` and `round-6-snapshot-manifest.json` both carry `"round": "round-2"`. These are historical; note them, do not rewrite them.
- **P3-5: Round-7 artifacts.** After this round, persist `review/round-7/**` including `review-metadata.json`, update `redactions.json`, and re-capture `worktree-status` last.
- **P3-6: Linker warning.** The macOS `__eh_frame` warning stays ledgered against OX-284 final C.

## Item-by-item

| # | Item | Result |
|---|---|---|
| 1 | Round-6 P1-1, README round-4 report SHA | **Closed.** The README round-4 entry cites report `c20503e7b81fd243…` and labels `9168b3f0…` as the snapshot manifest. The ledger matches. `review_history` no longer claims an absent README correction, though attribution is inaccurate (P2-1). A paren is unbalanced in the same line (P3-1). |
| 2 | Round-6 P2-1, ledger round-5 report SHA | **Closed.** The ledger has the full `78046e09dc8e162ec2041251dabbe29274c96b025fa778956e2b1e008dc33688`. |
| 3 | Round-6 P3-1, README round-5 entry | **Closed.** The round-5 entry and resolutions are present. Round-6 entry missing (P3-2); "all four locations" not re-attributed (P2-1). |
| 4 | Round-6 P3-2/P3-3, §一 comma and §四 paren | **Not met.** The §四 bullet is balanced: "Push；其 A/B…PASS（report SHA `4531e2b8…`）均已记账；当前卡 FIX-OX-17（…审阅中）。" The §一 trailing comma remains, and records claim it is fixed (P1-1). |
| 5 | Round-6 P3-4, round-6 metadata and capture | **Closed.** See below. |
| 6 | Re-confirmations | **Confirmed.** See below. |

**Item 5, round-6 metadata and capture:**

- `review/round-6/review-metadata.json` and all of `review/round-6/**` appear in `worktree-status.stdout`.
- `redactions.json` covers them. Metadata is `1d706c72…`; report, prompt and snapshot manifest are `28025dbe…`, `34f5b177…` and `4659c6ee…`, matching `round-6-review-metadata.json`.
- The capture includes the final evidence set.

**Item 6, re-confirmations:**

- **A/B sources:** `A-source.rs` is the base blob `700d966a…` at `f60d771`. `B-final-source.rs` is the worktree file `b3dd61af…`.
- **Diff:** there is one test-only hunk at `@@ -525`:
  - `entered` barrier 10 s → 60 s
  - post-release window 10 s → 30 s
  - no assertion changes
  - the inline A and B sources differ only there; `empty_raw_…` stays at 10 s/10 s in both.
- **Runs:**
  - Base runs exit 0 / 101 / 101. Both failures are `Elapsed(())` at `:530:14`.
  - Fixed-source runs exit 0 / 0 / 0, at 210.66 / 204.86 / 214.48 s.
- **No production file changed:**
  - `production_source_files_touched: []`.
  - The diff stat shows only the test file, 2 insertions and 2 deletions.
  - There is no `Cargo.toml` or `Cargo.lock` change.
- **Acceptance criteria:**
  - AC-1, AC-3 and AC-4 are witness-asserted.
  - AC-2: the delivered prefix is witness-asserted; `LEASE_EXPIRED` is diagnostic-supported; no 410 is observable on this path.
  - AC-5 is not applicable.
- **Historical citations:**
  - `plan-20260920.md:275` and `:292` are cited at plan SHA `f1bb…`, and are unaffected by the one-line replacement at about line 465.
  - FIX-OX-15 line 48 is cited with SHA `0d46f8da…`.
- **Gates and diffs:**
  - fmt, clippy, `cargo build` and `cargo build --tests` exit 0 (linker warning only).
  - `cargo test --all` is separated out and owned by OX-284 final C.
  - `plan-file-diff.stdout` holds the real `<LargeFile>docs/plan/plan-20260920.md:19364:10000</LargeFile>` marker.
  - `code/A-B-source.diff` is the `diff -u` artifact, paired with exit 1. The `libra diff` output is kept separately in `code/libra-diff-src.stdout`.
- **Review history:** `review_history` lists rounds 1–6, and `review` points at round 7 (PENDING).
- **Plan-SHA phrases:** all four (header, §一, §二, §七) read exactly 当前计划 SHA `f1bb94367cfd8c878409d76ee53c9deca83f88f50de1b20d91119d7e689520f1`（本卡修订前 `24f1be3fe8e431f664ced7eeb03d403a7793ce393d69aa162d663071566b444c`）, with no `））`.
- **Release state:**
  - Builds report `mega2 v0.42.25`, so there was no version bump.
  - The branch line is `## main...origin/main` with nothing ahead, so nothing has been pushed.
  - There is no tag or Release.

## Required for round 8 (text-only)

1. Remove the trailing "，" in the §一 plan-20260920 row and close the cell with `|` (P1-1).
2. Qualify the round-4 and round-5 resolution attributions in `review_history` and the README (P2-1).
3. Fix the README round-4 parenthesis, add a round-6 README entry, and add the round-6 report SHA to the ledger (P3-1–P3-3).
4. Persist the round-7 artifacts, update `redactions.json`, and re-capture `worktree-status` last (P3-5).

VERDICT: FAIL
