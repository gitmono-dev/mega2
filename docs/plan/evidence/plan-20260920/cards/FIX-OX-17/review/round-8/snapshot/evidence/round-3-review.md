# FIX-OX-17 ER-05 review, round 3

## Summary

The blocking finding from round 2 is closed. Every FIX-OX-17 occurrence in `context/plan-status.md` now describes the delivered failure-branch path, and none claims an empty diff or an evidence-only delivery. The card text is amended, the A/B mechanics are sound, and the gates are green.

The verdict is still FAIL. Four items that round 2 explicitly required for round 3 were not done, or were done in only one file:

- The tree-state capture was not refreshed. It is byte-identical to round 2 and does not show the plan amendment that this round relies on. This one is blocking.
- `review_history` is still empty, and the `review` block is stale.
- The ledger count drift remains.
- One evidence-only claim survives in the round-1 metadata summary.

The code change itself is acceptable.

## Findings

### P1 (blocking)

**P1-1: No current tree-state capture exists, and the archived one contradicts the claimed amendment.**

- **The capture is the round-2 file.**
  - `verification/source-restoration/worktree-status.stdout` hashes to `3c2d4b3a0cdf…`, the same value recorded in `evidence/round-2-snapshot-manifest.json`.
  - `evidence/redactions.json` records that same hash for both `review/round-2/snapshot/…/worktree-status.stdout` and the live `verification/…/worktree-status.stdout`.
  - Round 2's "Required for round 3" item 6 was "Re-capture `worktree-status`". It was not done.
- **What the stale capture omits:**
  - ` M docs/plan/plan-20260920.md`. The amendment that resolves round-2 P2-1 is not visible in any tree capture.
  - Every `review/round-2/**` artifact.
  - `verification/source-restoration/plan-amendment.{diff,exit}`.
- **The plan-diff record contradicts the amendment.**
  - `verification/source-restoration/plan-file-diff.stdout` is still the round-1 capture: 0 bytes, exit 0. It records "no plan diff".
  - `verification/source-restoration-check.json` `plan_amendment.note` explains that `libra diff` elides the plan as `<LargeFile>`. An elided large file would print a marker, not empty output, so the archived file does not show that elision.
  - The packet therefore contains two plan-diff records that disagree. Only one of them, `plan-amendment.diff`, reflects the change.
- **The README's claim about the capture is false.** `evidence/README.md` § Review record, round-1 resolutions, says "`worktree-status.stdout` is captured after the evidence set is final (P3-2)". That is no longer true.
- **Consequence:** the review cannot confirm the write set of the tree that will actually be committed. That covers plan-20260920.md, plan-status.md and the test file, plus the absence of any `Cargo.toml` or `Cargo.lock` change and the `## main...origin/main` with nothing ahead (request item 8). The precise-commit step under ER-07/GC-12 depends on this.
- **Fix:**
  - Re-run `libra status --short --branch` after all round-3 edits are final.
  - Either refresh `plan-file-diff.*` so it shows the actual `<LargeFile>` output, or delete it and point to `plan-amendment.diff`.
  - Update the README sentence.

### P2

**P2-1: `review_history` is not populated, and the `review` block is stale (round-2 P3-2 unresolved; request item 4).**

- `evidence/verification-results.json` still has `"review_history": []`.
- Its `review` block still says `"round": 2, "verdict": "PENDING", "report_sha256": null`. Round 2 returned literal FAIL with report SHA `3f76992f2094…`.
- The SHAs needed to fill this in are already in the packet:
  - Round 1: `5a4505ff660d…`, from `round-1-review-metadata.json` and from `redactions.json` for `review/round-1/claude.stdout`.
  - Round 2: `3f76992f2094…`, from `round-2-review-metadata.json`.
- **Fix:** list rounds 1 and 2 with verdict, report SHA, prompt SHA and snapshot manifest SHA. Set `review` to round 3.

**P2-2: The round-1 metadata still claims the AC-5 evidence-only path.**

- In `evidence/round-1-review-metadata.json`, the P2-4 `status` field was corrected.
- Its `summary` field still reads "…the card now takes its AC-5 evidence-only path with no source delta."
- This is the round-2 P1-1 defect class (item 4 of that finding), now confined to one field and contradicted by its own `status`.
- **Fix:** restore the original round-1 wording, e.g. "the executed hardening path was not reflected in the card text".

**P2-3: The ledger's plan SHA is stale after this card's amendment.**

- `context/plan-status.md` still names the current plan as `24f1be3fe8e4…` in four places:
  - the header ("当前计划候选 SHA 为 …");
  - §一 row;
  - §二 table ("当前计划状态 SHA …");
  - §七.
- After the amendment the plan is `f1bb94367cfd…`, per `context/plan-source-sha256.txt` and `source-restoration-check.json` `plan_sha256`.
- `evidence/README.md` § Card amendment also calls `24f1be3f…` "the R32-reviewed, M0-bound value". The ledger binds R32 PASS to `9d77fdd0…` and M0 to `ff3c36e9…`. `24f1be3f…` is only the "候选" (candidate) SHA.
- **Fix:**
  - Record `f1bb9436…` as the current plan SHA in the ledger.
  - Keep `24f1be3f…` as the pre-amendment value.
  - Correct the README label.

### P3

- **P3-1: Count drift is not corrected (round-2 P3-3; request item 4).**

  | Location in `plan-status.md` | Pending count | Locally-accepted |
  |---|---|---|
  | header | 295 | "12 张 locally-accepted（FIX-OX-08/11/01/02/09/04/05/12/13/14/15）" enumerates only 11 cards; FIX-OX-16 is missing |
  | §一 | 295 | 12 |
  | §二 table | 296 | 12 (12 + 296 = 308 ≠ 307) |
  | §三 | 296 | 12 |
  | §四 next-action | 295 | lists 11 cards |
  | §七 | 296 | lists 11 cards |

  Two further pre-existing contradictions sit in the same rows and should be fixed in the same edit:
  - §一 still says "未 Push、未 bump、未发布" although FIX-OX-15 and FIX-OX-16 were pushed.
  - §一 still says FIX-OX-14 "当前证据/状态提交待完成" (evidence/status commit still pending), which is stale.

- **P3-2: The changed-line count is wrong.** `source-restoration-check.json` `line_changes: 4` and the README's "4 changed lines" do not match `plan-amendment.diff`. That diff has one hunk, -1/+1, with one card line replaced.
- **P3-3: Residual mechanism attribution in the FIX-OX-18 handoff.** The `plan-status.md` § FIX-OX-17 handoff bullet says "本卡测得发布启用路由校验约 9–10 s/请求" (this card measured route verification at about 9–10 s per request).
  - What was measured is `oneshot_ms` under the publication-enabled fixture. Nothing measured route verification.
  - The README's phrasing ("a publication-enabled cost") is correct; the ledger bullet should match.
  - The README's "the source contains a `revalidate_access` call before and after each backend poll" is a source-reading claim with no production source in the packet. It is correctly not called an observation; this is acceptable as worded.
- **P3-4: The narrative timeline conflates two runs.** README "Withdrawal and reinstatement" says the draft was withdrawn when "only a single isolated run (A-run1) was available and that run passed".
  - The pre-withdrawal isolated run is the round-1 `A-VER-1` (231.48 s, cited on the card).
  - `A-run1` (208.77 s) is the first run of the post-withdrawal three-run gate.
  - Name them distinctly.
- **P3-5: The A/B diff payload is mislabelled.** The packet's `code/A-B-source.diff` is the `libra diff` output (git-style header, round-2 SHA `99a3ccb1…`). It is paired with `evidence/A-B-source.diff.exit = 1`, which belongs to the `diff -u` artifact (`verification/A-B-source.diff`, SHA `01740ae6…`). The content is equivalent, but the labels are mixed.
- **P3-6: The linker warning.** The macOS `__eh_frame` linker warning in `cargo build` and `cargo build --tests` remains ledgered against OX-284 final C. This is noted for tracking only.

## Answers to the review items

1. **P1-1 (round 2): closed.** Every FIX-OX-17 occurrence in `plan-status.md` states the delivered path:
   - Base runs 0/101/101 and fixed-source VER-1 runs 0/0/0.
   - One test-only hunk changing two constants.
   - AC-5 not applicable.

   The occurrences are in the header, §一, §二 (both places), §三, §四 audit record and next-action, §六 DEP-OX-01, and §七. None claims an empty source diff or an evidence-only delivery. The only residual evidence-only wording is in the round-1 metadata (P2-2).
2. **P2-1 (round 2): the card is amended; documentation is partial.**
   - `context/task-card.md` `Current evidence（2026-10-11 修订）` records the 0/101/101 baseline, the `Elapsed(())` failure at `:530:14`, the production-semantics checks, the barrier margin, the test-only fix with VER-1 at 0/0/0, and AC-5 as not applicable.
   - `source-restoration-check.json` `plan_amendment` has the before SHA (`24f1be3f…`), the after SHA (`f1bb9436…`), and the `diff -u`-instead-of-`libra diff` rationale.
   - Its changed-line count is wrong (P3-2).
   - Its "<LargeFile>" rationale is not backed by the archived `plan-file-diff` output, and no tree capture shows the plan modified (P1-1).
3. **P2-2 (round 2): closed with a minor residual.** No packet text claims an instrumented or observed `revalidate_access`. The README, the ledger and the card all label the mechanism a hypothesis. The handoff wording residual is P3-3.
4. **P3-1 / P3-2 / P3-3 (round 2):**
   - P3-1: stated, with a timeline mix-up (P3-4).
   - P3-2: **not done** (P2-1).
   - P3-3: **not done** (P3-1).
5. **P1-1 / P1-2 (round 1): re-confirmed.**
   - All three base runs and all three VER-1 runs are archived with exit, stdout and stderr files, and the hashes match the manifest.
   - The citations to `plan-20260920.md:275` and `:292` are consistent with the amendment: it is a single-line replacement at line ~465, so earlier line numbers are unchanged.
   - The FIX-OX-15 attribution line 48 is cited with SHA `0d46f8da…`.
   - The line numbers themselves cannot be checked from the excerpted `plan-historical-records.md`.
6. **Round-2 P3-4: recorded and sound.** README § Review record gives the 30 s window rationale: it is a margin choice, not a measurement. A source that continues into another held poll still trips the timeout and panics, and `tail_polls == 0` plus the case-correct prefix are still asserted.
7. **A/B mechanics: confirmed.**
   - `A-source` matches the base blob `700d966a…` at `f60d771`. `B` matches the worktree at `b3dd61af…`.
   - The diff is one hunk at @@ -525 changing two constants (10→60 s and 10→30 s). The inline A and B sources differ only there.
   - Base runs are 0/101/101, with both failures at `.unwrap()` `:530:14` `Elapsed(())`. Fixed-source runs are 0/0/0.
   - No production file changed (`production_source_files_touched: []`; the diff stat shows only the test file).
   - AC-1, AC-3 and AC-4 are witness-asserted.
   - AC-2: the prefix is witness-asserted and the `LEASE_EXPIRED` termination is diagnostic-supported. No 410 is observable on this path.
8. **Gates and release state:**

   | Gate | Exit | Notes |
   |---|---|---|
   | `cargo +nightly fmt --all --check` | 0 | empty output |
   | `cargo clippy --all-targets --all-features -- -D warnings` | 0 | |
   | `cargo build` | 0 | pre-existing linker warning |
   | `cargo build --tests` | 0 | pre-existing linker warning |
   | `source .env.test && cargo test --all` | not run | stated as owned by OX-284 final C |

   - The compiles report `mega2 v0.42.25`, so there was no version bump.
   - No tag or Release artifacts exist.
   - The "no Cargo change, not ahead of origin" check rests on the stale capture (P1-1). It must be re-confirmed on a fresh capture.

## Required for round 4

1. Re-capture `worktree-status` on the final tree. Fix or remove the stale `plan-file-diff.*` and correct the README sentence (P1-1).
2. Populate `review_history` for rounds 1 and 2 with their SHAs, and update the `review` block (P2-1).
3. Fix the P2-4 `summary` in `round-1-review-metadata.json` (P2-2).
4. Update the ledger's current plan SHA to `f1bb9436…` and correct the README's R32/M0 label (P2-3).
5. Reconcile the ledger counts to one consistent set: 12 locally-accepted including FIX-OX-16, and a pending count that sums to 307 (P3-1).
6. Optionally address P3-2 through P3-5.

VERDICT: FAIL
