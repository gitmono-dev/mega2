# FIX-OX-17 ER-05 review, round 2

## Summary

The evidence directory and the status records describe two different deliveries:

- **The evidence files describe a test fix.** `evidence/README.md`, `ab-manifest.json`, `verification-results.json`, `verification/source-restoration-check.json`, the A/B diff, `tracked-source-diff.stdout` and `worktree-status.stdout` all say the same thing. The base source failed 2 of 3 focused runs. The card took its "any failure → keep investigating" branch. It delivers one test-only hunk: the `entered` barrier goes from 10 s to 60 s and the post-release window from 10 s to 30 s. The fixed source then passed 3 of 3 runs.
- **The status records and this review request describe an evidence-only delivery.** `context/plan-status.md` § "FIX-OX-17 当前卡审计记录", `evidence/round-1-review-metadata.json` and request item 3 all say the card is AC-5 evidence-only with no source delta.

The evidence files are the accurate account. The live ledger, as written, would commit false statements about what was run and what changed. The evidence-side work is otherwise good:

- The three-run gate is archived.
- The baseline failure is now archived directly.
- The historical records are cited exactly.
- AC-2 is restated correctly.
- The gates are green.

The verdict is FAIL until the records agree with the evidence.

## Findings

### P1 (blocking)

**P1-1: The live ledger and the round-1 metadata misstate the delivered path and the results.**

`context/plan-status.md` § FIX-OX-17 当前卡审计记录, first and last bullets, plus the same phrase repeated in the header snapshot, §一, §二, §三, §四 and §七, asserts four things:

1. "本卡不给 `snapshot_raw_blob_tests.rs` 增加任何 hunk（`libra diff -- src` 为空；A-source 与最终工作树同为 `700d966a…`）". It also asserts "‘屏障加固’草稿已按 AC-5 撤回且无残留 hunk".
   - This is contradicted by `verification/source-restoration/tracked-source-diff.stdout`, which shows the @@ -525 hunk.
   - It is contradicted by `tracked-source-diff-stat.stdout`, which shows 2 insertions and 2 deletions.
   - It is contradicted by `worktree-status.stdout`, which shows ` M src/api/router/snapshot_raw_blob_tests.rs`.
   - It is contradicted by `source-restoration-check.json`, where the worktree is `b3dd61af…` and `b_source_matches_worktree: true`.
   - It is contradicted by `code/B-worktree-snapshot_raw_blob_tests.rs`, which has 60 s / 30 s.
2. "VER-1 连续三次…三次均 exit 0", framed as runs on unchanged source. In fact, the three base runs are `A-run1/2/3.exit` = 0 / 101 / 101, both failures `Elapsed(())` at `:530:14`. The three passing runs are on the fixed source.
3. "AC-5 满足". AC-5 requires three consecutive passes on the base source with no source delta. Neither condition holds. `verification-results.json` `acceptance_criteria.AC-5` and README § Acceptance criteria correctly say "not applicable as written".
4. `round-1-review-metadata.json` marks P2-4 "resolved by the AC-5 path" and P3-4 "moot after the withdrawal". Both statuses are false for the same reason.

Request items 3 and 7 ask me to confirm these claims. **I cannot.** The `src` diff is not empty, `A-source` does not equal the worktree, and the draft does leave a hunk. Only "no production file changed" holds.

**Fix:**
- Rewrite every FIX-OX-17 occurrence in `plan-status.md` to the failure-branch delivery. It should state the base results 0/101/101, the fixed-source VER-1 results 0/0/0, the one test-only hunk with its two constants, and that AC-5 is not applicable.
- Correct the P2-4 and P3-4 statuses in the round-1 metadata.
- Re-capture `worktree-status` after these edits.

### P2 (non-blocking individually; fix with P1-1)

**P2-1: Card text is still not reconciled. Round-1 P2-4 remains open.**
- `verification/source-restoration/plan-file-diff.stdout` is 0 bytes, and the plan SHA is still `24f1be3f…`.
- `context/task-card.md` "Current evidence" still records only the pass (exit 0 / 194.12 s) and the empty-diff closure condition.
- The card now has a reproduced 2/3 focused failure and a test-side fix delivered under the failure branch. Under ER-03/ER-10, the in-axis fix should be recorded on the card before acceptance, either in Current evidence or in a revision-history note.
- Choosing the failure branch over AC-5 is correct: `README.md` § "Why the card did not close on an empty diff". What is missing is the card amendment.

**P2-2: "Observed `revalidate_access`" still asserts an uninstrumented measurement. Round-1 P2-2 is partially open.**
- README § Diagnostics item 1 correctly says `revalidate_access` is not instrumented and labels the mechanism a hypothesis.
- But README § Follow-ups says the card "observed a `revalidate_access` call before and after each backend poll".
- `plan-status.md` FIX-OX-17 bullet 3 and the FIX-OX-18 handoff bullet say "并观察 poll 前后各一次 `revalidate_access`".
- Nothing in `diagnostic-source.rs` or `diagnostic-run.stderr` measures that.
- **Fix:** reword to "hypothesised" or remove it.

### P3

- **P3-1:** README § A/B notes "An earlier draft … was withdrawn when three runs appeared to pass (run 1 only). The full three-run gate then reproduced the failure and reinstated it." Apart from that sentence, there is no record of a withdrawal and reinstatement, and the sentence itself is unclear. State plainly what was withdrawn, when, and on what evidence; this narrative is also what produced the ledger error in P1-1.
- **P3-2:** `verification-results.json` has `review_history: []` even though round 1 (FAIL, report SHA `5a4505ff…`) exists. Populate it.
- **P3-3:** `plan-status.md` has pre-existing count drift across sections: "12 张 locally-accepted" vs "11 张卡本地验收", and "295" vs "296 pending", and the 12-card enumeration lists only 11. This predates the card and is not its scope, but correct it while editing the ledger.
- **P3-4:** The `__eh_frame` linker warning in `cargo build` and `cargo build --tests` is ledgered against OX-284 final C. That is acceptable under the FIX-OX-14/15/16 precedent; recorded here only for tracking.

## Answers to the review items

1. **P1-1 (round 1): fixed.**
   - `VER-1-run1/2/3.exit` are each 0.
   - Each stdout reports `1 passed; 0 failed; 2463 filtered out`, at 210.66 s / 204.86 s / 214.48 s.
   - `verification-results.json` `runs[]` lists all three, plus A-run1..3.
   - These passing runs are on the **fixed** source, not the base.
2. **P1-2 (round 1): fixed.**
   - `README.md` § Historical records and `ab-manifest.json` `historical_records` cite `plan-20260920.md:275` and `:292` (SHA `24f1be3f…`), and `FIX-OX-15/.../full-suite-failure-attribution.md` line 48 (SHA `0d46f8da…`).
   - The failure is no longer claimed from an unarchived record. `A-run2/3` reproduce it directly.
   - The reconciliation with the card's Current evidence is in the README only, not on the card (see P2-1).
3. **Path change: not as described.**
   - `libra diff -- src` is not empty.
   - `A-source` (`700d966a…`) differs from the worktree (`b3dd61af…`).
   - The hunk is present.
   - No production file changed: `production_source_files_touched: []`, and the diff stat shows only the test file.
   - AC-5 is not correctly applied in the ledger. The withdrawal is recorded inconsistently (P1-1, P3-1).
   - The delivery the evidence actually supports (failure branch, test-only timing fix, assertions unchanged) is sound.
4. **P2-1 (round 1): fixed in the evidence files.**
   - The README, `verification-results.json` and the ledger all state AC-2 as a witness-asserted prefix plus diagnostic-supported `LEASE_EXPIRED`.
   - They also state explicitly that no 410 is observable on this path.
5. **P2-2 (round 1): partially fixed.** See P2-2 above for the remaining "observed `revalidate_access`" wording.
6. **P2-3 (round 1): fixed in substance.**
   - The FIX-OX-18 handoff bullet exists in `plan-status.md` § FIX-OX-17 当前卡审计记录, including the environmental check required before calling the cost a production characteristic.
   - It must be carried through the P1-1 rewrite, and its "观察" wording fixed per P2-2.
7. **P2-4 (round 1): not reconciled.** The card text is unchanged, and the ledger describes a path that was not executed (P1-1, P2-1).
8. **P3 items from round 1:**

   | Item | Status | Evidence |
   |---|---|---|
   | P3-1 redactions card label | Fixed | `redactions.json` `"card": "FIX-OX-17"` |
   | P3-2 worktree capture | Fixed | `worktree-status.stdout` includes the README, manifests, `redactions.json`, `verification-results.json`, `source-restoration-check.json` and its own files; must be re-captured after the P1-1 edits |
   | P3-3 probe wording | Fixed | README and ledger state that the 5 s probe exits 0 and does not reproduce exit 101 |
   | P3-4 30 s window rationale | Stated, still applicable | README § Review record; the window is **not** withdrawn, it is in the delivered hunk |
   | P3-5 linker warning | Ledgered | Assigned to OX-284 final C |

9. **Re-confirmations:**
   - **AC-1** (`counts.assert(1, prefix_length)` after the barrier), **AC-3** (`tail_polls == 0`, `drops == 1`) and **AC-4** (both budgets 0) are all asserted in `code/B-worktree-snapshot_raw_blob_tests.rs`. The assertions are identical to A.
   - **Gates:**

     | Gate | Exit | Notes |
     |---|---|---|
     | `cargo +nightly fmt --all --check` | 0 | empty output |
     | `cargo clippy --all-targets --all-features -- -D warnings` | 0 | |
     | `cargo build` | 0 | linker warning only |
     | `cargo build --tests` | 0 | linker warning only |
     | `cargo test --all` | not run | stated as owned by OX-284 final C |

   - **Release state:** compiles report v0.42.25, so no version bump. The worktree branch line is `## main...origin/main` with nothing ahead, so nothing has been pushed. There are no tag or Release artifacts.

## Required for round 3

1. Rewrite the FIX-OX-17 entries in `plan-status.md` to the actual failure-branch delivery: base runs 0/101/101, fixed-source VER-1 runs 0/0/0, one test-only hunk, AC-5 not applicable.
2. Correct the P2-4 and P3-4 statuses in `round-1-review-metadata.json`.
3. Amend the card text in `plan-20260920.md` (P2-1).
4. Remove the "observed `revalidate_access`" wording (P2-2).
5. Clarify the withdrawal narrative (P3-1) and populate `review_history` (P3-2).
6. Re-capture `worktree-status`.

The review request should also stop asserting an empty source diff.

VERDICT: FAIL
