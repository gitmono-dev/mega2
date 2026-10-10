# R30 review: plan-20260920 (expected SHA `6eab19b4…aae73`)

I couldn't compute SHA-256 hashes because I only had Read, Glob and Grep. I reviewed the snapshot files as given and relied on your manifest for the hash. I made no edits and executed no cards.

R30 does not pass. One P1 and two P2 findings remain, so the plan gate stays closed. Most of the R29 findings are closed (details at the end), but adding FIX-OX-23 opened two new gaps, and one part of the R29 history finding is still open.

## Blocking findings

**P1. FIX-OX-23 is a REL-OX-01 member, but no evidence-batch card checks it, and the final ledger counts are stale.**
- FIX-OX-23 appears in REL-OX-01 (line 197). It does not appear in any evidence-batch card: OX-300..OX-370, OX-380, OX-390 or OX-381 (searched lines 6109–8960).
- The batches only add up to the old total. The OX-370 ledger covers 282 cards, OX-380 adds 5, OX-390 adds 5, OX-381 adds 5, and OX-371 itself checks 4 more. That is 301.
- OX-371 AC-18 and its Deliverables (lines 8946, 8951), and OX-383 AC-1 and Current evidence (lines 8966, 8968), still require exactly 301 members. With 304 members, the correct figure is 304 − OX-371 − OX-383 = 302.
- As written, OX-371 AC-18 will either fail every time or pass with FIX-OX-23 never audited. Either way, OX-284 cannot release as planned.
- Fix:
  - Add FIX-OX-23 to OX-304 (the FIX-OX-18/19/20 batch, currently 16/20 ACs; adding four makes 20/20), or create a new batch card and put it in the chain, REL-OX-01 and the granularity table.
  - Change 301 to 302 in OX-371 AC-18/Deliverables and OX-383 AC-1/Current evidence.

**P2. The only branch that lets FIX-OX-19 start would make FIX-OX-23's own test fail, which the plan's rules forbid.**
- Line 203 says every single-test command must show `running 1 test` and `1 passed; 0 failed`, and it applies to the whole plan.
- In FIX-OX-23's anchor-mismatch branch (AC-3), VER-1 necessarily ends with exit 101 and `0 passed; 1 failed`. The batch audits also require "原始退出码 0" for every applicable A/B gate (for example, OX-304 AC-5 at line 6268).
- No exception is defined. So in the one outcome that unblocks FIX-OX-19, FIX-OX-23 cannot become `locally-accepted`, and FIX-OX-19 depends on it (line 559). The R29 P1 fix therefore deadlocks in its key branch.
- Fix:
  - Make FIX-OX-23 VER-1 a diagnostic check with an expected result per branch. For the anchor branch: exit 101, `running 1 test`, `0 passed; 1 failed`, and the panic text contains the 500 status and the anchor/active-lease message.
  - Add a scoped exception to line 203 and to the batch-audit exit-0 criteria for that branch.
  - Have FIX-OX-19 depend on FIX-OX-23 being accepted plus its AC-3 evidence.

**P2. The R29 history finding is only partly closed.**
- Line 56 and the revision-history row at line 9677 say what changed in `46d83eb6…`. They still don't say what changed from `ff3c36e9…` to `c28381c8…`.
- Nor do they say which gate allowed the card A/B work for FIX-OX-11, 01, 02 and 09 under those two SHAs, neither of which was ever passed. Line 82 only allows A/B to resume when the latest PASS manifest SHA matches the plan SHA.
- The plan file also writes card status into itself, which line 79 forbids: FIX-OX-08 (line 740) and FIX-OX-11 (line 4667) show `locally-accepted`. Meanwhile FIX-OX-01, 02 and 09 show `pending` in the plan but are accepted in `plan-status.md`.
- Line 86's wording ("五张已验收卡保持其账本证据") doesn't settle whether those five acceptances are confirmed or need re-review.
- Fix:
  - State the `ff3c→c283` change, or confirm it was status/evidence only.
  - Say explicitly whether an R30 PASS confirms the five acceptances or sends them back for re-review.
  - Either remove the Lifecycle values written into the plan file or record them as a named exception.

## Non-blocking (P3)

1. **FIX-OX-19 serialization label disagrees with the table.** The card says `writeset=序列化于 FIX-OX-18` (line 572); the granularity table says FIX-OX-23 (line 9188).
2. **Line 154 says FIX-OX-12 "不消费此 scaffold", which is wrong.** FIX-OX-12's write set (line 357) and FIX-OX-02 (line 241) both say it consumes the scaffold. The raw-blob tests build the shared `Fixture` (`snapshot_raw_blob_tests.rs:101,192`), so setting a 3600s lease needs `FixtureOptions.lease_seconds`. It should say "future consumer, not a checkpoint hunk owner".
3. **FIX-OX-03 doesn't index FIX-OX-23.** Current evidence still says "FIX-OX-01..22" (line 714), and the VER-2 `rg` pattern leaves out FIX-OX-23 (line 724).
4. **The risk register leaves out FIX-OX-23** (line 9541).
5. **The R29 revision-history row is cut off from its table** by the blank line 9676.
6. **FIX-OX-23 AC-4 and the Description use different conditions.** AC-4 says "96 次 lookup 全部通过"; the Description says lookups complete *and* the test passes. Align them, since the test has assertions after the loop.
7. **FIX-OX-18 VER-1 is a single run**, though that test failed in its only focused run (line 293). Consider repeating it like VER-2/3/4.
8. **FIX-OX-19 VER-1 doesn't use the exact test path / `--exact` form** that FIX-OX-23 uses.
9. **`plan-status.md` has stale wording.** Line 60 says "两张卡本地验收" but five are accepted. Lines 13 and 49 put SHA `46d83…` next to "M0 R28 PASS", which could read as R28 covering `46d83…`. The post-PASS sync should fix both.

## What checks out

- **Card and member counts:** there are 307 `### Task` headings. I enumerated REL-OX-01 as 196 implementation + 108 docs/audit = 304, which equals 307 minus OX-284, OX-283 and OX-20.
- **Order:** the serial chain, test matrix, trace table and M1 all include FIX-OX-18 → FIX-OX-23 → FIX-OX-19 → FIX-OX-21. OX-284 is the only bump, push, tag, Release and versioned Docker publish.
- **FIX-OX-23 setup:**
  - Its three result branches are stated in the Description and AC-3/4/5.
  - The test path resolves in source: `snapshot_content_tests.rs:150` → `snapshot_rooted_metadata_tests.rs:10` → `snapshot_reader_retention_tests.rs:56`.
  - The `new_in_publication_mode_with_options(true, 0, &[], false, true, …)` arguments match the test's current `new_with_pg_config(true)` (`snapshot_content_tests.rs:645-711`).
- **FIX-OX-19:**
  - It has 8 ACs.
  - The lease change is no longer part of it.
  - Its write set names the real retention test file.
  - It cannot start before the anchor 500 is reproduced.
- **FIX-OX-18:**
  - VER-2, VER-3 and VER-4 each require three consecutive passing runs.
  - AC-5 and AC-6 are separate.
  - FIX-OX-03 VER-3's full serial run covers failures that only show up in suite order.
- **FIX-OX-17 and FIX-OX-12:** FIX-OX-17's evidence is updated and the empty-delta acceptance path is defined (AC-5). FIX-OX-12 now cites the checkpoint focused timings.
- **Review history:**
  - Review-log rows for R28 and R29 are present and tied to their exact SHAs.
  - FIX-OX-04 is shown as diagnosed but not accepted.
  - The post-PASS README/`plan-status.md` sync step is specified at lines 79, 82 and 135.
  - The plan does not claim R28 passed any newer bytes.

VERDICT: FAIL
