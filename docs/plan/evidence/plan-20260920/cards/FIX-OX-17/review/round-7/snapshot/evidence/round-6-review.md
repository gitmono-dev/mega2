# FIX-OX-17 ER-05 review, round 6

## Summary

The code change, the A/B mechanics and the gates are acceptable, as in rounds 3–5. Round-5 items P1-1, P3-1 and P3-4 are closed in the live ledger and the tree capture.

**Verdict: FAIL.** Round-5 P2-1 is only half fixed. The ledger now cites the correct round-4 report SHA, but `evidence/README.md` still cites the snapshot-manifest SHA as the round-4 report. `evidence/verification-results.json` `review_history[4].resolutions` says this was corrected "in the README and the ledger", which is not true.

This is the same failure pattern that made round-5 P1-1 blocking: an authoritative evidence record claims a correction that was not made. The ledger has also introduced a new defect: an empty placeholder where the round-5 report SHA should be. All remaining work is text-only, and no re-run is needed.

## Findings

### P1 (blocking)

**P1-1: Round-5 P2-1 is not closed in the README, and `verification-results.json` says it is.**
- `evidence/README.md`, § Review record, "Round 4" entry still reads: "literal `VERDICT: FAIL` (report SHA-256 `9168b3f072b2dad3…` for the snapshot; findings were text-only)".
  - `9168b3f0…` is the round-4 snapshot-manifest SHA, per `round-4-review-metadata.json` `snapshot_manifest_sha256`.
  - The round-4 report SHA is `c20503e7b81fd243…`, per the same file's `report_sha256` and `redactions.json` for `review/round-4/claude.stdout`.
- `evidence/verification-results.json`, `review_history[4].resolutions` (round 5) says: "round-4 report SHA corrected to c20503e7 in the README and the ledger". That is false for the README.
- **Fix:**
  1. Change the README round-4 entry to report SHA `c20503e7b81fd243…`, and if you keep the manifest SHA, label it as "snapshot manifest `9168b3f0…`".
  2. Re-hash the README and update `redactions.json`.

### P2

**P2-1: The ledger has an empty placeholder for the round-5 report SHA.**
- `context/plan-status.md`, § "FIX-OX-17 当前卡审计记录", ER-05 bullet, reads: "round 5 literal `VERDICT: FAIL`（report SHA `…`，四项 SHA 措辞中三项仍畸形…）".
- The value should be `78046e09dc8e162e…`, per `round-5-review-metadata.json` `report_sha256`, `verification-results.json` `review_history[4]`, and `redactions.json` for `review/round-5/claude.stdout`.

### P3

- **P3-1: Round 5 is missing from the README review record.**
  - `evidence/README.md` § Review record has entries for rounds 1–4 only. There is no round-5 entry and no "Round-5 resolutions" line, while `verification-results.json` and the ledger both record round 5.
  - The "Round-4 resolutions" line and `review_history[3].resolutions` still say the SHA phrasing was normalised "in all four locations" during round 4. Round 5 showed that was false at the time. Either attribute the normalisation to round 5 or qualify it.
  - The entry order is still Round 2 → Round 3 → Round 4 → Round 1.
- **P3-2: §一 row ending.**
  - The plan-20260920 row now has exactly four cells, so round-5 P3-1 is closed.
  - The last cell ends with a dangling "…ER-05 round-6 审阅中，", with a trailing comma and no closing `|`. It still renders, but the sentence is truncated.
- **P3-3: Unmatched bracket in §四.**
  - The next-action bullet reads: "FIX-OX-16 已在 `f60d771` 提交并 Push（A/B、fmt、clippy、build、ER-05 round-2 literal PASS（report SHA `4531e2b8…`）；当前卡 FIX-OX-17（…审阅中）。"
  - There are three "（" and two "）", so the "Push（" parenthetical never closes.
  - This is not a SHA phrase, so it does not fail request item 1, but it is the same class of defect.
- **P3-4: Historical round labels.**
  - `evidence/round-5-snapshot-manifest.json` carries `"round": "round-2"`, the same mislabel as round-5 P3-3 found in the round-3 manifest.
  - Both are historical artifacts. Note them; do not rewrite them.
- **P3-5: Linker warning.** The macOS `__eh_frame` linker warning stays ledgered against OX-284 final C.

## Item-by-item

| # | Item | Result |
|---|---|---|
| 1 | Round-5 P1-1, SHA phrasing | **Closed.** Details below. |
| 2 | Round-5 P2-1, round-4 report SHA | **Not met.** The ledger is correct; the README still cites `9168b3f0…`, and `verification-results.json` claims otherwise (P1-1). |
| 3 | Round-5 P3-1, §一 cells | **Closed.** There are four cells and no FIX-OX-14 cell. The truncated ending is P3-2. |
| 4 | Round-5 P3-4, round-5 artifacts | **Closed.** Details below. |
| 5 | Re-confirmations | **Confirmed.** Details below. |

**Item 1, SHA phrasing.** The header, §一, §二 and §七 each read exactly:

当前计划 SHA `f1bb94367cfd8c878409d76ee53c9deca83f88f50de1b20d91119d7e689520f1`（本卡修订前 `24f1be3fe8e431f664ced7eeb03d403a7793ce393d69aa162d663071566b444c`）

- The hashes are full, there is one parenthetical per location, and the extra "为" in the header is gone.
- I found no `））` anywhere in the file.

**Item 4, round-5 artifacts.**
- `review/round-5/review-metadata.json` and the rest of `review/round-5/**` appear in `worktree-status.stdout`.
- `redactions.json` covers them, including metadata `c3fa1900…`, report `78046e09…` and snapshot manifest `39f853f5…`. These match `round-5-review-metadata.json`.
- The capture includes the final evidence set: `redactions.json`, `verification-results.json`, `plan-file-diff.stderr` and the `tracked-source-diff-stat.*` files.

**Item 5, re-confirmations.**

- **A/B sources and diff:**
  - `A-source.rs` is the base blob `700d966a…` at `f60d771`. `B-final-source.rs` is the worktree file `b3dd61af…`.
  - `code/A-B-source.diff` is the `diff -u` artifact and is paired with exit 1. The `libra diff` output is kept separately in `code/libra-diff-src.stdout`.
  - There is one test-only hunk at `@@ -525`: the `entered` barrier goes from 10 s to 60 s and the post-release window from 10 s to 30 s. No assertions changed. The `empty_raw_…` test stays at 10 s/10 s in both A and B.
- **Runs:**
  - Base runs exit 0 / 101 / 101. Both failures are `Elapsed(())` at `:530:14`.
  - Fixed-source runs exit 0 / 0 / 0, at 210.66 / 204.86 / 214.48 s.
- **No production file changed:** `production_source_files_touched: []`, the diff stat lists only the test file, and there is no `Cargo.toml` or `Cargo.lock` change.
- **Acceptance criteria:**
  - AC-1, AC-3 and AC-4 are asserted by the witness test.
  - AC-2: the delivered prefix is witness-asserted. The `LEASE_EXPIRED` termination is diagnostic-supported, and no 410 is observable on this path.
  - AC-5 is not applicable.
- **Historical citations:** `plan-20260920.md:275` and `:292` are unaffected by the single-line replacement at about line 465. FIX-OX-15 line 48 is cited with SHA `0d46f8da…`.
- **Plan diff:** `plan-file-diff.stdout` holds the real `<LargeFile>docs/plan/plan-20260920.md:19364:10000</LargeFile>` marker.
- **Gates:**

  | Gate | Exit | Notes |
  |---|---|---|
  | `cargo +nightly fmt --all --check` | 0 | |
  | `cargo clippy --all-targets --all-features -- -D warnings` | 0 | |
  | `cargo build` | 0 | linker warning only |
  | `cargo build --tests` | 0 | linker warning only |
  | `source .env.test && cargo test --all` | not run | owned by OX-284 final C |

- **Review history:** `review_history` lists rounds 1–5, and `review` points at round 6.
- **Release state:**
  - The builds report `mega2 v0.42.25`, so there was no version bump.
  - The branch line is `## main...origin/main` with nothing ahead, so nothing has been pushed.
  - There is no tag or Release.

## Required for round 7 (text-only)

1. Fix the round-4 report SHA in the README to `c20503e7b81fd243…` (P1-1).
2. Fill in the round-5 report SHA `78046e09dc8e162e…` in the ledger audit record (P2-1).
3. Add a round-5 entry and round-5 resolutions to the README review record. Optionally re-attribute the "all four locations" claim from round 4 to round 5 (P3-1).
4. Optionally fix the trailing "，" in the §一 row and the unmatched "Push（" in §四 (P3-2, P3-3).
5. Persist the round-6 artifacts, update `redactions.json`, and re-capture `worktree-status` last.

VERDICT: FAIL
