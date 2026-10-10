# R31 review: plan-20260920 (expected SHA `7ba76ae3…3e09e`)

**R31 does not pass.** Two P2 findings remain: one R30 P1 repair is incomplete, and one R30 P2 sub-item that the plan reports as fixed is still open. Every other R29 and R30 item I checked is closed.

I only had Read, Glob and Grep, so I couldn't compute the SHA-256 of the plan or the snapshot manifest. I reviewed the snapshot files as given and relied on your manifest for the hashes. I made no edits and ran no task cards. I read the full archived R30 report and checked each repair against the current plan text, not against the revision-history claims.

## Blocking findings

**P2-1. The OX-371 ledger arithmetic still adds up to 301, not 302.**
- FIX-OX-23 is now in OX-304 batch 05 (line 6288), so the OX-300..OX-370 chain covers one more card. Counting it:
  - Batch members: 3 + 3×3 + 4 + 65×3 + 2 = 213.
  - Earlier batch audit cards OX-300..OX-369: 70.
  - So the OX-370 cumulative ledger has **283** cards.
- 283 + OX-380 (5) + OX-390 (5) + OX-381 (5) + 4 more (OX-370 itself, OX-240, OX-238, OX-381 itself) = **302**. That matches AC-18 (line 8950) and OX-383 (lines 8970, 8972).
- But OX-371's Description (line 8930) still says "OX-370 的 282 卡累计 ledger". AC-1 (line 8933) still requires "覆盖原冻结成员集的 282 张卡", and its list of cards that must not appear in the old ledger doesn't mention FIX-OX-23.
- As written, the card's parts sum to 301 while AC-18 requires 302. An auditor could fail AC-1, or treat FIX-OX-23 as wrongly added to the old ledger. Either way, the counts are not internally consistent, which R30 P1 required.
- **Fix:** change 282 to 283 in the OX-371 Description and AC-1, and state that the OX-370 ledger includes FIX-OX-23 via OX-304.

**P2-2. FIX-OX-08 and FIX-OX-11 still write live status into the plan, but the plan says this was fixed.**
- Line 740 (FIX-OX-08) reads `` `in-progress` / `locally-accepted` (2026-10-09 20:21:58 UTC; …) ``. Line 4667 (FIX-OX-11) reads `` `in-progress` / `locally-accepted` ``.
- Both cards also keep checked `[x]` AC/VER boxes (lines 744–754 and 4671–4681), live evidence text (line 742), and write sets of "本卡状态/证据" (lines 756, 4683).
- That contradicts:
  - line 5: every card's Lifecycle/Acceptance is at the default;
  - line 79: status goes only in `plan-status.md` and evidence only under `evidence/`;
  - revision row line 9682: "Lifecycle 字段回归默认值".
- A grep confirms 305 of 307 cards are at the default; these two are not.
- Line 56 now explains the `ff3c→c283→46d83` chronology, and says R31 PASS does not certify the five accepted cards. But it treats "状态/证据回写不另触发 M0" as an exception that isn't written into the binding M0 gate text in lines 79 and 82. Line 82 still requires the plan SHA to match the latest PASS before A/B can resume.
- **Fix:**
  - Reset both cards to `` `pending` / 空 ``, uncheck the boxes, and keep the evidence only in `plan-status.md` and `evidence/plan-20260920/cards/FIX-OX-08|11/`.
  - Add the historical c283/46d83 writeback exception explicitly to lines 79/82 as a named, closed exception.

## Non-blocking (P3)

1. **Review-log preamble is out of date** (line 9584). It still says this revision needs an "R30 Claude literal PASS", and its list of FAIL rounds stops at R25/R29. It should say R31 and include R30.
2. **FIX-OX-19 VER-1 wording is ambiguous** (line 558). "按 FIX-OX-23 的分支门…判定" could be read as letting the exit=101 exception apply to FIX-OX-19 too. State that FIX-OX-19's B side must exit 0 with `running 1 test` and `1 passed; 0 failed`. Only its A-side control reproduces the anchor-mismatch 500.
3. **The granularity exception field doesn't match the table.** FIX-OX-23's card Granularity has a non-N/A `exception=` (line 540). Table row 9191 shows `N/A`, and line 9169 says all cards have `exception=N/A`, even though the table claims to be generated from the cards. Since that field is meant for EX-* waivers, move the note out of it or reconcile the table and line 9169.
4. **FIX-OX-23's own state is unclear in the "other result" branch** (AC-5, line 527 and OX-304 AC-17). Say explicitly that FIX-OX-23 itself stays blocked/not accepted in that branch, not only its successors.
5. **The M0 milestone row says "所有卡仍 pending"** (line 9520), while five cards are locally accepted. Reword it as the original M0 condition, or as "no new card execution".

## What checks out

- **Counts and order:**
  - 307 `### Task` headings, 307 `**Granularity:**` lines, and 307 granularity table rows.
  - REL-OX-01 lists 196 + 108 = 304 members, which is 307 minus OX-284, OX-283 and OX-20.
  - The serial chain, test matrix, trace table and M1 all include FIX-OX-18 → FIX-OX-23 → FIX-OX-19 → FIX-OX-21. The dependency edges I checked resolve with no cycles.
- **R30 P1:**
  - OX-304 now has 20/20 ACs (card and table row 9409).
  - Its AC-17..20 cover FIX-OX-23 for each result branch: A's known 410, plus exactly one declared B branch.
- **R30 P2 (expected failure):**
  - Line 203 now carries one exact exception: FIX-OX-23 VER-1 in the anchor branch only, with `running 1 test`, `0 passed; 1 failed`, exit=101, plus HTTP 500 and the PostgreSQL anchor/active-lease mismatch.
  - The all-pass branch requires exit 0 and `1 passed; 0 failed`, and keeps FIX-OX-19/21 blocked until a reviewed plan change.
  - Any other result keeps them blocked and goes through FIX-OX-04 AC-3/AC-4. No unrelated test failure is excused.
- **R30 P2 (chronology):** apart from P2-2 above, the history is exact. R28 PASS is bound only to `ff3c36e9…` (lines 56, 9613; `plan-status.md:13`). R31 PASS does not certify the five accepted cards (lines 5, 56, 86).
- **R30 P3s closed:**
  - FIX-OX-19 serialization label (lines 572, 9192).
  - Line 154 FIX-OX-12 scaffold wording.
  - FIX-OX-03 indexes include FIX-OX-23 (lines 714, 724).
  - Risk register includes FIX-OX-23 with test-only scope (line 9545).
  - R29/R30 revision rows sit inside the table (lines 9681–9682).
  - FIX-OX-23 AC-4 matches its Description.
  - FIX-OX-18 VER-1 requires three consecutive passes.
  - FIX-OX-19 VER-1 uses the exact path with `--exact --test-threads=1`.
  - README line 48 and `plan-status.md` lines 13, 49, 60 and 64 show 5/302 and "R31 待审".
- **R29 items closed:**
  - FIX-OX-23 is split from FIX-OX-19.
  - FIX-OX-19 now writes to the real retention test file.
  - FIX-OX-18 has separate AC-5/AC-6 and three-run VERs.
  - FIX-OX-17 has an explicit empty-delta acceptance path (AC-5).
  - FIX-OX-12 cites the 614.68s and 616.69s checkpoint timings.
  - FIX-OX-04 is recorded as diagnosed but not accepted.
- **Gates and release:**
  - No card execution is authorized before an R31 literal PASS (lines 5, 86, 9682).
  - No test listing is presented as a test pass.
  - OX-284 is the only point for the version bump, push, tag, GitHub Release and versioned Docker publish.

One unrelated note: the session reported that the claude.ai Google Calendar connector needs authorization in your claude.ai connector settings before it can be used. It wasn't needed for this review.

VERDICT: FAIL
