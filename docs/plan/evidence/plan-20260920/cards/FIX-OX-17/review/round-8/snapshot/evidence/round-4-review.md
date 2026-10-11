# FIX-OX-17 ER-05 review, round 4

## Summary

**The code change, A/B mechanics and gates are confirmed again and are acceptable.** The round-3 blocking item is closed: the tree capture is fresh, and the plan-diff and A/B diff payloads are correctly labelled.

**The verdict is FAIL.** The ledger edits for round-3 items 4 and 5 were applied with a faulty find-and-replace:

- The live ledger now records this card's own plan amendment as a no-op.
- Several statements the request says are corrected are still present.

All remaining work is text-only. No test re-run is needed.

## Findings

### P1 (blocking)

**P1-1: The ledger mislabels the current plan SHA as the pre-amendment value, in four places.**

- **§一 row** (`context/plan-status.md`), progress cell: "当前计划 SHA `f1bb9436…`（本卡修订前为 `f1bb9436…（本卡修订前 `24f1be3f…`）`）".
- **§二 table:** the same nested form.
- **§七 note:** the same nested form.
- **Header:** "当前计划候选 SHA 为 `f1bb9436…（本卡修订前 `24f1be3f…`）`". The SHA and the parenthetical sit inside one backtick span, with backticks nested inside it.

In three places the ledger says the plan SHA *before* this card's amendment was `f1bb9436…`, which is the *after* value. Read literally, the card did not change the plan. That contradicts `verification/source-restoration/plan-amendment.diff` and the `plan_amendment` block in `source-restoration-check.json`, where the SHA goes from `24f1be3f…` to `f1bb9436…`.

This is the round-2 P1-1 defect class: the authoritative ledger misstates what this card changed. It also fails request item 4: `24f1be3f…` does appear only as the pre-amendment value, but `f1bb9436…` is also labelled pre-amendment. The nested backticks also break the Markdown rendering.

**Fix:** in every location, write exactly `当前计划 SHA \`f1bb9436…\`（本卡修订前 \`24f1be3f…\`）`, using full hashes and no nesting.

### P2

**P2-1: The round-3 review metadata is not saved in the tree.**

- `evidence/round-3-review-metadata.json` is in the packet.
- `review/round-3/` in `worktree-status.stdout` contains only `claude.{exit,stderr,stdout}`, `prompt.md` and `snapshot/**`. There is no `review-metadata.json`, although rounds 1 and 2 each have one.
- `redactions.json` has no entry for it either.

So either the file was written after the "final" capture, or it exists only in the packet. In both cases the precise commit would omit the round-3 FAIL record.

**Fix:** write `review/round-3/review-metadata.json`, add it to `redactions.json`, then re-capture `worktree-status`.

### P3

**P3-1: The counts and enumerations are still inconsistent (request item 5 is not met).**

- **FIX-OX-16 listed twice:** the header and the §一 status cell both list "FIX-OX-08/11/01/02/09/04/05/12/13/14/15/16/16". That is 13 entries for 12 cards.
- **FIX-OX-16 missing from §四:** the next-action list names 11 cards ("…FIX-OX-13、FIX-OX-14、FIX-OX-15 已 locally-accepted，307 张卡中 295 张仍 pending"). 11 + 295 is 306, not 307.
- **FIX-OX-16 missing from §七:** the note ("…FIX-OX-15 已 `in-progress / locally-accepted`，295 张仍 `pending`") also lists 11.

**P3-2: Stale statements remain.**

- **FIX-OX-14 commit pending:** the §一 row still ends with an extra fifth cell, "FIX-OX-14 … 当前证据/状态提交待完成。" The request says this was corrected. The extra cell also breaks the 4-column table.
- **R32 sentence in §四:** "当前 R32 门禁提交 `4ee15179…`，无 Push/bump/发布。" is a surviving variant of "未 Push、未 bump、未发布", even though FIX-OX-15 and FIX-OX-16 were pushed.

**P3-3: The review status lags by one round everywhere.**

- **`verification-results.json`:** the `review` block says round 3 `PENDING` with `report_sha256: null`. Round 3 is finished: literal FAIL, report SHA `3430990b…`.
  - This does meet the request's wording, but it repeats the round-3 P2-1 pattern.
  - Add round 3 to `review_history` and point `review` at round 4.
- **`plan-status.md`:** says "ER-05 round-3 审阅中" in the header, §一–§四 and §六. The FIX-OX-17 audit record's ER-05 bullet ends at "round 3 修订中" and does not record the round-3 FAIL.
- **`README.md` § Review record:** covers round 1 only.

**P3-4: The linker warning is tracked only.** The macOS `__eh_frame` warning in `cargo build` and `cargo build --tests` stays ledgered against OX-284 final C.

## Item-by-item

| # | Item | Result |
|---|---|---|
| 1 | Round-3 P1-1 | **Closed.** The capture (`d41c66c7…`) is new, not the round-2 hash. It shows `M docs/plan/plan-20260920.md`, `M docs/plan/plan-status.md`, `M src/api/router/snapshot_raw_blob_tests.rs`, all of `review/round-2/**` and `review/round-3/**`, and `plan-amendment.{diff,exit}`. `plan-file-diff.stdout` holds the `<LargeFile>docs/plan/plan-20260920.md:19364:10000</LargeFile>` marker. `code/A-B-source.diff` is the `diff -u` artifact, paired with exit 1, and the `libra diff` output is kept separately in `code/libra-diff-src.stdout`. Residual: P2-1. |
| 2 | Round-3 P2-1 | **Met as worded.** Rounds 1 and 2 are listed with verdict and report, prompt and manifest SHAs, and they match the metadata files. `review` points at round 3, which is now stale (P3-3). |
| 3 | Round-3 P2-2 | **Closed.** The P2-4 summary and status no longer claim the AC-5 path, and no other field does. |
| 4 | Round-3 P2-3 | **Not met.** README is correct: it calls `24f1be3f…` the prior candidate and binds R32 to `9d77fdd0…` and M0 to `ff3c36e9…`. The ledger is not (P1-1). |
| 5 | Round-3 P3-1 | **Not met** (P3-1, P3-2). |
| 6 | Round-3 P3-2 | **Closed.** `line_changes: 1`, and the diff is -1/+1. |
| 7 | Round-3 P3-3 | **Closed.** The ledger says "测得 `oneshot` 约 9–10 s/请求 … 未对路由校验本身插桩". This matches `oneshot_ms` of 10257 / 10273 / 9131 / 9107. |
| 8 | Round-3 P3-4 | **Closed.** `A-VER-1` (231.48 s) is named separately from `A-run1` (208.77 s). |
| 9 | Round-3 P3-5 | **Closed.** |
| 10 | Re-confirmations | See below. |

**Re-confirmations (item 10):**

- **A/B sources:** `A-source.rs` is the base blob `700d966a…`, and `B-final-source.rs` is the worktree `b3dd61af…`. The diff is one hunk at `@@ -525` changing 10→60 s and 10→30 s, with no assertion changes. The inline A and B sources differ only there.
- **Runs:** base runs are 0 / 101 / 101, both failures at `:530:14` with `Elapsed(())`. Fixed-source runs are 0 / 0 / 0, at 210.66 / 204.86 / 214.48 s.
- **No production file changed:** `tracked-source-diff-stat` shows only the test file, and the capture shows no `Cargo.toml` or `Cargo.lock` change.
- **Acceptance criteria:**
  - AC-1, AC-3 and AC-4 are witness-asserted.
  - AC-2's prefix is witness-asserted; the `LEASE_EXPIRED` termination is diagnostic-supported, and no 410 is observable on this path.
  - AC-5 is not applicable.
- **Historical citations:** `:275` and `:292` are unaffected by a single-line replacement at about line 465. FIX-OX-15 line 48 is cited with SHA `0d46f8da…`.
- **Gates:** fmt, clippy, `cargo build` and `cargo build --tests` all exit 0. `cargo test --all` is honestly deferred to OX-284 final C.
- **Release state:** the build reports v0.42.25, the branch line is `## main...origin/main` with nothing ahead, and there is no tag or Release.

## Required for round 5 (text-only)

1. Fix all four SHA phrasings to the single form in P1-1.
2. Persist `review/round-3/review-metadata.json` and add it to `redactions.json`.
3. Deduplicate FIX-OX-16 in the header and §一. Add FIX-OX-16 to the §四 and §七 lists.
4. Delete the extra "FIX-OX-14 … 提交待完成" cell. Fix the "无 Push/bump/发布" sentence in §四.
5. Record round 3 FAIL and round 4 in `verification-results.json`, the ledger audit record and the README review record.
6. Re-capture `worktree-status` last.

VERDICT: FAIL
