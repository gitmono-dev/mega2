# FIX-OX-17 ER-05 review, round 5

## Summary

The code change, the A/B mechanics and the gates are acceptable, as in rounds 3 and 4. Round-4 items P2-1 and P3-1 are closed, and P3-3 is mostly closed.

The verdict is still FAIL. The round-4 blocking item, the plan-SHA phrasing, is fixed in only one of its four locations. The other three now contain a duplicated parenthetical and an unmatched closing bracket. The README and `verification-results.json` both say the phrasing was "normalised in all four places", which is not true. Two smaller record defects remain as well. All remaining work is text-only, and no re-run is needed.

## Findings

### P1 (blocking)

**P1-1: Round-4 P1-1 is not closed. Three of four SHA phrasings are malformed, and the record claims they were fixed.**

These are the four locations in `context/plan-status.md`:

| Location | Current text | Status |
|---|---|---|
| Header ("当前快照" paragraph) | 当前计划 SHA 为 `f1bb…`（本卡修订前 `24f1…`） | Correct. The extra "为" is acceptable but differs from the requested exact form. |
| §一, plan-20260920 row, progress cell | 当前计划 SHA `f1bb…`（本卡修订前 `24f1…`）（本卡修订前 `24f1…`）） | Duplicated parenthetical and unmatched "）" |
| §二 table | 当前计划状态 SHA `f1bb…`（本卡修订前 `24f1…`）（本卡修订前 `24f1…`）） | Same defect |
| §七 note | 当前计划 SHA `f1bb…`（本卡修订前 `24f1…`）（本卡修订前 `24f1…`）） | Same defect |

- The facts are no longer inverted: `f1bb…` is never labelled as the pre-amendment value.
- The text still fails request item 1, which requires that each phrase read exactly 当前计划 SHA `f1bb…`（本卡修订前 `24f1…`） with no nesting.
- This looks like the find-and-replace was run again on already-edited text.
- The false claim appears in two places:
  - `evidence/README.md` § Review record, "Round-4 resolutions": "SHA phrasing normalised … in all four places".
  - `evidence/verification-results.json`, `review_history[3].resolutions`.

**Fix:**
1. Replace each of the three malformed strings with the single form.
2. Optionally drop "为" from the header.
3. Then check with a literal search that `（本卡修订前` appears exactly once per location and that no `））` remains.

### P2

**P2-1: Round 4's report SHA is recorded as the snapshot-manifest SHA.**

- `evidence/README.md` § Review record, Round 4, says "report SHA-256 `9168b3f072b2dad3…` for the snapshot".
- The ledger's FIX-OX-17 audit record, ER-05 bullet, says "round 4 literal `VERDICT: FAIL`（report SHA `9168b3f072b2dad3…`".
- `9168b3f0…` is the round-4 **snapshot manifest** SHA, per `round-4-review-metadata.json` `snapshot_manifest_sha256` and `redactions.json` for `review/round-4/snapshot/snapshot-manifest.json`.
- The actual report SHA is `c20503e7b81fd243…`, per `round-4-review-metadata.json` `report_sha256`, `verification-results.json` `review_history[3]`, and `redactions.json` for `review/round-4/claude.stdout`.
- Two of the three review records therefore mis-cite the round-4 report.

**Fix:** use `c20503e7…` as the report SHA in both places.

### P3

**P3-1: The stray fifth cell in the §一 row is still there (round-4 P3-2, partly fixed).**
- The "提交待完成" wording is gone.
- The row still ends with an extra cell: `…OX-284 是唯一末尾 patch 发布点. | FIX-OX-14 A/B、fmt、clippy 已通过，Claude ER-05 round-2 literal PASS（report SHA \`e7c193c3…\`）。`
- That makes five cells in a four-column table.
- **Fix:** delete the trailing `| FIX-OX-14 …` cell.

**P3-2: The README review record has stale and out-of-order text.**
- The "Round-4 resolutions" line says "rounds 1–3 recorded in `review_history`", but rounds 1–4 are recorded there.
- The Round 1 entry comes after the Round 4 entries.
- This does not block.

**P3-3: `evidence/round-3-snapshot-manifest.json` has the wrong round label.**
- It carries `"round": "round-2"`, although its payload hashes are round-3 values (for example `plan-status.md` `ff933f0f…`).
- It is a historical artifact, so correct it only if it gets rewritten for another reason.

**P3-4: The final capture must include round 5.**
- The current `worktree-status.stdout` correctly has no `review/round-5/**`.
- After this round, the round-5 prompt, report and metadata must be persisted.
- `redactions.json` must cover them.
- `worktree-status` must be re-captured again before the precise commit.

**P3-5: macOS linker warning.** The `__eh_frame` warning remains assigned to OX-284 final C.

## Item-by-item

| # | Item | Result |
|---|---|---|
| 1 | Round-4 P1-1, SHA phrasing | **Not met.** Correct in the header only; §一, §二 and §七 are malformed (P1-1). |
| 2 | Round-4 P2-1, round-3 and round-4 metadata | **Closed.** See below. |
| 3 | Round-4 P3-1, FIX-OX-16 enumerations and counts | **Closed.** See below. |
| 4 | Round-4 P3-2, stray cell and no-push sentence | **Partial.** The no-push sentence is fixed and the extra cell remains (P3-1). |
| 5 | Round-4 P3-3, review history | **Mostly closed.** The round-4 report SHA is mis-cited (P2-1). |
| 6 | Round-4 P3-4, linker warning | **Noted.** Remains with OX-284 final C. |
| 7 | Re-confirmations | **Confirmed.** See below. |

**Item 2, metadata:**
- `review/round-3/review-metadata.json` and `review/round-4/review-metadata.json` are both in `worktree-status.stdout`.
- Both are covered by `redactions.json`, at `b0cdf6da…` and `fea5f4d3…`.

**Item 3, enumerations:**
- The header and §一 each list FIX-OX-16 exactly once.
- §四 and §七 both include FIX-OX-16.
- Every section reads 12 locally-accepted and 295 pending, which totals 307.

**Item 4, no-push sentence:**
- §四 now reads "FIX-OX-15 与 FIX-OX-16 已本地提交并 Push，仍无 bump/tag/Release".
- No "未 Push" or "无 Push" variant remains.

**Item 5, review history:**
- `verification-results.json` `review_history` lists rounds 1–4. The verdicts and report, prompt and manifest SHAs match the metadata files.
- `review` points at round 5 with status PENDING.
- The ledger and README record rounds 1–4 and "round 5 审阅中".

**Item 7, re-confirmations:**
- **Tree capture:** `worktree-status.stdout` shows `## main...origin/main` with nothing ahead.
  - It shows ` M` for `docs/plan/plan-20260920.md`, `docs/plan/plan-status.md` and `src/api/router/snapshot_raw_blob_tests.rs`.
  - It shows all of `review/round-1..4/**` and `plan-amendment.{diff,exit}`.
  - There is no `Cargo.toml` or `Cargo.lock` change.
- **Plan diff:** `plan-file-diff.stdout` holds the real `<LargeFile>docs/plan/plan-20260920.md:19364:10000</LargeFile>` marker.
- **A/B diff:** `code/A-B-source.diff` is the `diff -u` artifact and is paired with exit 1. The `libra diff` output is separate, in `code/libra-diff-src.stdout`.
- **A/B sources:**
  - `A-source.rs` is the base blob `700d966a…`, and `B-final-source.rs` is the worktree file `b3dd61af…`.
  - There is one test-only hunk at `@@ -525`, changing the barrier from 10 s to 60 s and the window from 10 s to 30 s.
  - The inline A and B sources differ only there. The `empty_raw_…` test stays at 10 s/10 s in both, which is consistent.
- **Runs:**
  - Base runs are 0/101/101, and both failures are `Elapsed(())` at `:530:14`.
  - Fixed runs are 0/0/0, at 210.66 / 204.86 / 214.48 s.
- **No production file changed:** `production_source_files_touched: []`, and the diff stat lists only the test file.
- **Acceptance criteria:**
  - AC-1, AC-3 and AC-4 are witness-asserted.
  - AC-2: the delivered prefix is witness-asserted. The `LEASE_EXPIRED` termination is diagnostic-supported, and no 410 is observable on this path.
  - AC-5 is not applicable.
- **Historical citations:** `plan-20260920.md:275` and `:292` are cited at SHA `f1bb…`, which is consistent with a single-line replacement at line ~465. FIX-OX-15 line 48 is cited with SHA `0d46f8da…`.
- **Gates:**

  | Gate | Exit | Notes |
  |---|---|---|
  | `cargo +nightly fmt --all --check` | 0 | |
  | `cargo clippy --all-targets --all-features -- -D warnings` | 0 | |
  | `cargo build` | 0 | linker warning only |
  | `cargo build --tests` | 0 | linker warning only |
  | `cargo test --all` | not run | separated out, owned by OX-284 final C |

- **Release state:** builds report v0.42.25. There is no version bump, tag, Release or push by this card.

## Required for round 6 (text-only)

1. Rewrite the §一, §二 and §七 SHA phrases to exactly 当前计划 SHA `f1bb94367cfd…`（本卡修订前 `24f1be3fe8e4…`）, using full hashes. Then correct the "normalised in all four places" claims in the README and `verification-results.json` (P1-1).
2. Correct the round-4 report SHA to `c20503e7…` in the README and the ledger (P2-1).
3. Delete the extra FIX-OX-14 cell in §一 (P3-1).
4. Persist the round-5 artifacts, update `redactions.json`, and re-capture `worktree-status` last (P3-4).

VERDICT: FAIL
