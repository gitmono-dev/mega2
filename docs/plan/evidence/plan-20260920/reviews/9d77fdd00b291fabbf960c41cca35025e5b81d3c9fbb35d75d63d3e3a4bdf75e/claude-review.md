I'm returning **VERDICT: PASS** for R32 on plan SHA `9d77fdd0…bdf75e`. There are no P0, P1 or P2 findings. Every R29, R30 and R31 finding is closed in the current card text, and I found seven non-blocking P3 items.

I had only Read, Glob and Grep, so I could not compute SHA-256 hashes. I relied on your manifest for the plan and snapshot hashes. I edited nothing and ran no task card.

## R31 P2 closures

**P2-1, the OX-371 ledger count: closed.** I rebuilt the count from the 71 batch Deliverables lists (OX-300..OX-370):
- **Batch members:** 3 + 4 (batch 05: FIX-OX-18, FIX-OX-23, FIX-OX-19, FIX-OX-20) + 68×3 + 2 = **213**.
- **Previous batch audit cards** (OX-300..OX-369): **70**.
- **OX-370 ledger:** 213 + 70 = **283**.
- **No omission or duplicate.** I compared the 213 members in order against the REL-OX-01 lists (line 197): 19 FIX implementation cards, 162 OX implementation cards and 32 docs/audit cards.
- **Cards correctly left out of the OX-370 ledger (21):**
  - OX-370 itself, OX-371 and OX-383.
  - The other ledger cards: OX-380, OX-390 and OX-381.
  - OX-240, OX-238, OX-372..OX-379, FIX-OX-21/22, OX-382, OX-385 and OX-386.
  - 304 − 21 = 283.
- **Final total:** 283 + OX-380 (5) + OX-390 (5) + OX-381 (5: OX-377/378/379/380/390) + OX-371's own four (OX-370, OX-240, OX-238, OX-381) = **302**. That equals 304 minus OX-371 and OX-383.
- **Text now matches:** the OX-371 Description (line 8928) and AC-1 (line 8931) say 283 and include FIX-OX-23 via OX-304. AC-18 (line 8948), the Deliverables (line 8953) and OX-383 (lines 8968, 8970) say 302.
- **OX-304** has 20/20 ACs (card line 6294; AC-17..20 cover FIX-OX-23).

**P2-2, FIX-OX-08/11 state in the plan: closed.**
- Lines 740 and 4666 read `pending` / 空. All 307 Lifecycle lines are at the default, and the file has no `- [x]` checkboxes.
- The live evidence is gone, and both write sets list only `.github/workflows/docker.yml` (lines 755, 4681).
- Line 79 names both writebacks by full SHA and commit: `c28381c8…` (commit `346fe3c`) and `46d83eb6…` (commit `a5ba8fe`). It calls them closed historical exceptions that changed no design, dependency or release scope, are not a reusable process, and authorize no future writebacks or card starts.
- Line 82 says these exceptions do not bypass the gate requiring a literal PASS on the new SHA.
- Actual state lives in `plan-status.md` (lines 13, 49, 64): 5 locally-accepted, 302 pending.

## R31 P3 closures

1. **Review-log preamble** (line 9582) records R31 FAIL and says the current gate is R32.
2. **FIX-OX-19 VER-1** (line 558) requires the B side to exit 0 with `running 1 test` and `1 passed; 0 failed`. It says the exit=101 branch applies only to FIX-OX-23's own diagnosis.
3. **FIX-OX-23 Granularity** has `exception=N/A` (line 540), matching table row 9189 and line 9167.
4. **FIX-OX-23 AC-5** (line 527) and OX-304 AC-17 keep FIX-OX-23 itself and its successors blocked/not accepted in `plan-status`.
5. **M0 milestone** (line 9518) keeps the five separately evidenced accepted cards and pauses new card execution until R32 PASS.

## R29/R30 items still closed

- **FIX-OX-23 vs FIX-OX-19:** FIX-OX-23 is test-only, with a single fixture call point and no production file. The production SQL fix is in FIX-OX-19, which has 8/8 ACs and writes to the real retention test file.
- **One expected-failure exception** (line 203): only the FIX-OX-23 anchor branch, with exit=101, `0 passed; 1 failed`, HTTP 500 and the anchor/active-lease mismatch. No unrelated failure is excused.
- **R28 PASS** is bound only to `ff3c36e9` (lines 56, 9613; `plan-status.md` line 13).
- **FIX-OX-18:** AC-5 and AC-6 are separate, and VER-1..4 each require three consecutive runs.
- **FIX-OX-17:** current focused result plus the empty-delta acceptance path (AC-5).
- **FIX-OX-12:** cites the 614.68s and 616.69s timings; line 154 calls it a future consumer of the scaffold.
- **FIX-OX-03:** VER-2 includes FIX-OX-23; the risk register (line 9543) and revision rows 9680–9682 are inside their tables.

## Whole-plan checks

- **Card structure:** 307 task headings, 307 Lifecycle lines at default, 307 Granularity lines, 307 Dependencies lines and 307 granularity-table rows, all with `exception=N/A`.
- **Release group:** REL-OX-01 has 196 + 108 = 304 unique members and excludes OX-284, OX-283 and OX-20.
- **Order and dependencies:** the serial chain (line 174), test matrix (line 9486), trace table (line 9502) and M1 (line 9519) agree. The FIX dependencies I checked resolve with no cycles.
- **Execution gate:** no card execution is authorized before an R32 literal PASS (lines 5, 86, 9518, 9582).
- **Release:** OX-284 is still the only point for the patch bump, push, tag, GitHub Release and versioned Docker publish (lines 117, 165–166, 197).

## Non-blocking (P3)

1. **FIX-OX-08/11 no longer have a `Current evidence` field.** Template G-05 (`plan-template.md` line 401), which cannot be waived, expects a file:line anchor. Fix: add a neutral anchor such as `.github/workflows/docker.yml:<lines>` with no live state.
2. **Line 56 still says "状态/证据回写不另触发 M0" as a general present-tense rule.** Line 79 now limits this to the two closed historical writebacks. Fix: reword line 56 in past tense, limited to c283/46d83.
3. **Other cards still name `docs/plan/plan-20260920.md` for status or evidence**, for example FIX-OX-01/02/09 ("本卡状态/证据") and the OX-300..OX-383 audit Deliverables. Line 79 overrides this, but the wording conflicts. Fix: add a global note that these entries mean evidence under `evidence/plan-20260920/` or a fail-closed amendment.
4. **OX-371 AC-1's list of cards that must not appear in the old ledger leaves out FIX-OX-21/22**, which OX-390 covers. It also still says "原冻结成员集". Fix: add both cards to the list.
5. **Fault-recovery row 9530 still says "此前所有卡保持 pending"**, the same wording fixed in the M0 milestone. Fix: reword it to "不启动新卡".
6. **Line 5 says "若 R31 改动其验收条件".** Fix: make it R32, and state that the R32 edits to FIX-OX-08/11 (checkbox reset, evidence removal, narrower write set) leave their AC/VER unchanged, so no card re-review is triggered.
7. **OX-64 AC-3 (line 820) names `m20261008_000600` as the "current last" migration.** FIX-OX-21/22 add migrations before OX-64 runs, so that will no longer be true. The AC can still be met as written. Fix: say "the last migration at start of work".

The per-card evidence directories that `plan-status.md` cites (`evidence/plan-20260920/cards/FIX-OX-01|02|09|11/`) are not in the 937-file snapshot, so I could not open them. This does not affect the plan text.

VERDICT: PASS
