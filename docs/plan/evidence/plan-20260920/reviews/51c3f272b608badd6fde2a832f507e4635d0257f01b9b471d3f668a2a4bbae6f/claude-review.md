**R25 audit: plan-20260920.md, SHA `51c3f272…bae6f`**

**What I verified**
- **SHA and scope:** the plan's SHA-256 matches `51c3f272b608badd6fde2a832f507e4635d0257f01b9b471d3f668a2a4bbae6f`. There are exactly 306 `### Task` headings, and every card is still `pending`. Release tags split as 196 `REL-OX-01`, 109 `no-release` and 1 `independent` (OX-20). OX-284 is still the only final release point under G-12.
- **Where R25 changed the plan:** the R25 wording appears only in the gap table (line 43), the review header and table (56, 72), the status checklist (76) and the M0 hunk method (81). I found no R25 wording in any card, the DAG, the release sections or the user constraints. This copy has no 8109 snapshot, so I could not do a byte diff.
- **The transform is correct:**
  - Canonical hunk headers keep standard zero-context coordinates. In the fixtures, a pure deletion is written as `+N,0`, where N is the line before the deletion.
  - The Apple application patch adds 1 to `new_start` only when `new_count=0`. The plan keeps both byte sets and their SHAs separate and says the changed header must never be called a source coordinate.
  - I re-ran `apple-patch-preflight.py` on `patch 2.0-12u11-Apple`. All 7 cases PASS, and their canonical and application patch SHAs match the archived JSON.
- **Negative controls I added:**
  - **Without the +1, reverse patching is wrong:** delete-middle put `b` at the wrong line and still exited 0. Delete-start was rejected. A multi-line deletion was also misplaced with exit 0.
  - **With the +1, edge cases restore correctly:** multi-line deletions, deleting a file down to empty, a missing newline at end of file, and omitted-count headers all came back right.
  - **Forward patching needs no change:** applying canonical insertions and deletions forward is already correct. So limiting the transform to reverse application with `new_count=0` is the right scope.
- **The failed-attempt evidence is consistent:** 34 patch calls, all exit 0, none mentioning offset, fuzz or reject. The run stopped on the A-side hash mismatch, expected `5f5af640…` and actual `3d01eaf6…`. This shows the exact-SHA gate is the safeguard that actually works, and R25 keeps it.

**Findings**

**P1: The gate that restarts cards cannot be satisfied for the R25 SHA as written.**
- The last M0 checkbox (line 84) is the gate for starting any card A/B. It requires the plan SHA in the review manifest to equal the bytes of `docs/plan/plan-20260920.md` in the M0 dossier commit.
- The completion criterion (line 9575) likewise requires the PASS to be "bound to the unchanged plan SHA in the dossier".
- The M0 dossier commit `81f2e5c` contains the 8109 plan. Line 79 forbids rebuilding or replacing the checkpoint, and nothing defines another way to commit the new material: the 51c3 plan bytes, `reviews/51c3…/` and `r25-method-preflight/`. The execution copy is cloned from the checkpoint, so it also still carries the 8109 plan text.
- Read literally, an R25 PASS can never satisfy the gate. The only way to pass it is through the R24/8109 PASS, which certifies the method that just failed.
- **Fix:** define a signed, path-exact local amendment commit, with no card or version bump, that carries the new plan, its review and the preflight evidence. Bring it into the execution copy, with checks that the clone stays ahead and has not diverged. Then rewrite lines 84 and 9575 to bind the latest PASS manifest to the plan bytes in that commit, keeping the M0 checkpoint SHA as the WIP source.

**P3: The review log and change log have no rows for R24 PASS or the R25 amendment.**
The review-log preamble still says the latest R23 P2 must be re-reviewed. Add both rows.

**P3: The fixtures cover only single-line edits and include no negative control.**
The real Docker case is a replacement split into a deletion and an insertion with different owners, both at the same coordinate (`r08 g00/g01`). Add a multi-line deletion, that adjacent split shape applied through the ordered `-C` sequence, and an untransformed control that must fail. Hold the regenerated manifest to the same bar.

**P3: The plan should state how the transform orders with coordinate mapping and sorting.**
State that the +1 is applied after mapping to current-copy coordinates, that sorting uses the canonical mapped coordinate, and that full forward replay uses the canonical bytes. As specified these choices give the same result, but leaving them unstated invites divergent tooling.

The amendment's method is safe and correct, but the unresolved P1 leaves no satisfiable way to restart cards under the new SHA.

VERDICT: FAIL
