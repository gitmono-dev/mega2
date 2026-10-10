# R29 review of plan-20260920 (target SHA dc035b53…)

**Verdict: FAIL.** One P1 and two P2 findings block. I could not compute SHA-256 myself (no Bash), so I relied on the caller's manifest, where line 106 lists the expected plan hash and the file count is 921.

**What checks out.** 306 `### Task` headings. REL-OX-01 lists 303 members (195 + 108), which equals 306 minus OX-284/OX-283/OX-20, and FIX-OX-04/18/19 are in the right groups. The FIX segment of the serial chain (line 174) matches every card's `Dependencies` line, including FIX-OX-18 → FIX-OX-19 → FIX-OX-21 → FIX-OX-20 → FIX-OX-22. OX-284 remains the only patch bump, push, tag, Release and versioned Docker publish; its AC/VER and the override table are unchanged. FIX-OX-04's table is internally consistent: 11 persistent + 2 new = 13 current failures, 15 rows, every current failure has one owner, intermittent counts recorded, no raw log committed, no credentials. Assigning the two new reader-barrier failures to FIX-OX-18 is sound: both tests wrap real `Router::oneshot` calls in `with_rooted_reader_barriers` (verified at `snapshot_reader_retention_tests.rs:269` and `snapshot_rooted_metadata_tests.rs:348`), FIX-OX-18 AC-5 keeps the card blocked if the cause is not the hook, and no product defect is presumed. The explanation for the two now-passing historical failures is correct (FIX-OX-17's file is outside the checkpoint source diff; FIX-OX-14's expectation is a checkpoint hunk). FIX-OX-18 card counts (5/8, 4/8, 1/1) match audit row 9158; FIX-OX-19 (8/8, 1/8, 1/2) matches row 9159.

## Blocking findings

**P1. FIX-OX-19 bundles a test-only lease precondition with a production SQL fix whose root cause is not currently reproduced** (card lines 518–521; FIX-OX-04 line 296/304).
- The checkpoint tree never reached the anchor-mismatch 500; it hit 410 before finishing 96 lookups. The plan correctly says the 410 does not prove the anchor cause is fixed, but it equally does not prove it still exists. Yet FIX-OX-19 (production SQL, forward-only) and FIX-OX-21/22 (migrations keyed on the FIX-OX-19 family) proceed on that unreproduced cause. This contradicts the plan's own "prove the root cause first" rule used for FIX-OX-16/17/18 and FIX-OX-04 AC-3/AC-4.
- With the lease change inside the same card, the A side of A/B fails with 410, not the anchor 500, so A/B cannot show the SQL fix addresses the anchor root cause.
- AC-1 merges three predicates (lease covers 616.32s + margin; 96 lookups succeed; production lease contract unchanged). With 8 ACs listed, the real count is at least 9/8, which exceeds G-03 (precedent R9/R12).
- The 616.32s bound is the expiry point, not the completion time: the test aborted before finishing 96 lookups, so the actual duration is unknown and larger.
- Remediation: add a fixture-only card (e.g., FIX-OX-23, deps FIX-OX-18, before FIX-OX-19) following the FIX-OX-05/12 pattern, with an explicit lease such as 3600s and VER = the focused run plus three outcome branches: anchor 500 reproduces → FIX-OX-19 starts; passes → FIX-OX-19/21 blocked and plan amended; other error → new card per FIX-OX-04 AC-3/AC-4. Drop the lease clause from FIX-OX-19 AC-1, bringing it back to 8/8. Register the new card in the serial chain, REL-OX-01, M1, test/trace tables and the granularity table.

**P2. Review-status statements are inaccurate for their exact SHAs.**
- Line 56 ("当前 R28 修订待 Claude literal PASS"), line 76, line 86 ("R27 amendment commit … 之前 … 不继续/接受 FIX-OX-08 A/B") and the Review log paragraph at 9551 all say R28 is still pending review, while GAP-OX-03 (line 43) and the revision history (9642) record R28 PASS on `ff3c36e9…`. The Review log table has no R28 row.
- Lines 4/5, 56 and 9551 say all 306 cards are pending, but plan-status.md line 13 records FIX-OX-08/11/01/02/09 as locally-accepted and FIX-OX-04 has already executed VER-1/VER-2.
- Card evidence shows FIX-OX-11 was reviewed under plan SHA `c28381c8…` and FIX-OX-01/02/09 and the FIX-OX-04 run under `46d83eb6…` (FIX-OX-04 line 285). Neither SHA appears in the Review log, GAP-OX-03 or the revision history, and no plan-level Claude PASS or fail-closed amendment entry is recorded for them. Line 79 requires any plan-byte change to be recorded with old/new SHA and re-passed before cards resume. The plan must say what changed at those two SHAs and under which gate.
- Remediation: rewrite lines 56/76/86 and 9551 to the R28 PASS state, add an R28-Claude row to the Review log table, replace "all pending" with the real ledger pointer, and add revision-history rows for `c28381c8…` and `46d83eb6…` (trigger, scope, gate) or state explicitly that this R29 review covers them.

**P2. FIX-OX-18 cannot prove the determinism its AC-5 requires** (lines 496–501).
- AC-5 demands both new failures "确定性到达" the barrier, but VER-1..4 are single focused runs. FIX-OX-04 already recorded 1 pass/1 fail for the source-mutation test and 2/2 passes for the stale-reader test that still fails in the full run, and itself says a single pass does not close the case.
- Remediation: make VER-2/3/4 repeated runs (for example three consecutive focused runs, all must report `1 passed; 0 failed`), and state that order-dependent reproduction is covered by FIX-OX-03 VER-3's full serial run.

## Non-blocking (P3)

- **FIX-OX-19 write set names the wrong file for the call point** (line 533; ownership table line 154). The test builds its fixture at `snapshot_reader_retention_tests.rs:57` via `new_with_pg_config(true)`. An options-accepting constructor already exists at `snapshot_content_tests.rs:711`, so the lease can be set by editing only the retention test file. Remove `snapshot_content_tests.rs` from the FIX-OX-19 write set and from line 154, or state that a new constructor is required.
- **FIX-OX-17 evidence is stale** (line 465): it still reports focused exit=101/208.74s, while FIX-OX-04 reports exit=0/194.12s. Update the evidence and define how an implementation card with an empty delta is accepted (repeated focused runs, no A/B hunk).
- **FIX-OX-18 AC-5 is compound**: split into one AC per new test (count becomes 6/8) and move the "blocked if not a hook" sentence into Description.
- **Line 154 lists FIX-OX-12 alongside the checkpoint consumers FIX-OX-05/09**, but FIX-OX-12's file is not a checkpoint path. Mark it as a future consumer like FIX-OX-19.
- **FIX-OX-12 evidence** (line 349) cites only historical timings and `/tmp/mega2-raw-cold.log`; add the checkpoint focused results (614.68s, 616.69s) for traceability.

VERDICT: FAIL
