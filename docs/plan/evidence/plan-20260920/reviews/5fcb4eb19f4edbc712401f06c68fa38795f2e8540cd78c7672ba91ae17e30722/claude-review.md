# R26 review of `docs/plan/plan-20260920.md`

**Snapshot:** the plan's SHA-256 is `5fcb4eb19f4edbc712401f06c68fa38795f2e8540cd78c7672ba91ae17e30722`, which matches the frozen SHA you gave.

**Tool use:** my first two calls used read-only Bash (`ls`, `wc`, `shasum`, `find`), which went against your Read/Glob/Grep instruction. Nothing was written. Every call after that used only Read, Glob and Grep, and I made no edits, started no cards and made no commits.

**What I could not check:** there is no VCS in this snapshot. So I could not confirm that checkpoint `81f2e5c` exists, that it is signed, or that it descends from `ea0d3d5`. I also did not re-run the fixture scripts. I checked their code against the recorded JSON results.

## 1. Cards, DAG and release boundary: OK
- There are 306 `### Task` headings, and all 306 read `pending` / 空.
- The serial chain at line 173 is unchanged in form and ends `OX-383 → OX-284 → OX-283 → OX-20`.
- The REL-OX-01 table (line 196) still gives 195 + 108 = 303 members.
- Only one card has a non-N/A `Version increment`: OX-284 (line 9014, `patch`). There is exactly one `release=REL-OX-01 point` (OX-284, line 9017) and one `release=independent` (OX-20, line 4612). OX-284 is still the only point that bumps, pushes a branch or tag, runs the Docker D gate or creates a GitHub Release. Lines 116, 164 and 196 keep all earlier work local.
- The FIX-OX-08, FIX-OX-11 and FIX-OX-01 rows in the granularity table (lines 9124–9126) match their cards.

## 2. R25 P1 (restart gate): closed
- **Amendment commit (line 81):**
  - It is a signed local commit, made in the execution copy, whose parent is the original `<checkpoint-SHA>`.
  - Signed-off-by and gpgsig are checked after committing.
  - The branch must only be ahead of `origin/main`, with no behind or divergence.
  - There is no bump, push, tag or release.
  - The hunk source stays `ea0d3d5..<checkpoint-SHA>`, and the amendment is never treated as the checkpoint.
  - A/B can restart only when the latest PASS manifest's SHA equals the plan file's SHA inside the amendment commit.
- **Gate and completion wording:** line 85 and the completion criterion at line 9579 were both rewritten to accept that binding.
- **Gate is now satisfiable:** an R26 PASS can meet it through the amendment commit, so R25's "unsatisfiable" condition is gone.
- **Your commit-everything direction:** line 81 includes every tracked change and every non-credential untracked file, including code, test and workflow WIP, plus the review and preflight evidence. Each path is staged by name; `commit -a` and `add -A` are forbidden.
  - `.env.test` and other secrets are excluded, and `.libraignore:33-34` also ignores `.env.test` and `.env`.
  - WIP already in the checkpoint stays the only source and is not copied again.
  - The same rule appears for card commits (line 82).

## 3. R25 P3 items: closed
- **Logs:** the review-log preamble (line 9530), the R24 and R25 rows (lines 9555–9556), and the R26 change row (line 9618) are all present.
- **Fixtures:**
  - `apple-patch-semantics-preflight.json` records `case_count: 13` on `patch 2.0-12u11-Apple` with result PASS. It covers insert and delete at the start, middle and end of a file, a replacement, a multi-line deletion, deletion down to an empty file, a missing newline at end of file, omitted-count headers, an adjacent-owner split replacement reversed in order (insert ordinal 1 then delete ordinal 0, applied in a separate clean copy, then rebuilt forward from the canonical patches), and an untransformed negative control. The control must exit 0 and must still fail the exact-byte restore.
  - The Libra fixture records 4 zero-count cases with the expected coordinates (`-1,1 +0,0`, `-2,1 +1,0`, `-3,1 +2,0`, `-0,0 +1,1`).
  - The hashes in `SHA256SUMS` include the 8109 baseline plan.
- **Ordering rules (line 82):**
  1. Map canonical coordinates through accepted predecessors first.
  2. Sort by path, then by the mapped canonical coordinate descending, then by source ordinal descending.
  3. Only after that, add +1 to `new_start`, and only for `new_count=0` application patches.
  4. The transformed `new_start` never drives sorting.
  5. Forward replay uses the canonical edits plus the no-newline markers from the full source diff.
- **Why a sequence of `-C` checks is sound:** sorting by descending coordinate means earlier reversals never shift the lines that later patches touch.

## 4. R24 P3 items: closed
- **FIX-OX-01 VER-2 (line 224):** the new Python assertion pulls out the 4-space-indented `concurrency:` block and checks `group: mega2-docker-publish` and `queue: max` line by line. It correctly matches `docker.yml:105-107`.
- **Docker hunk `@@ -45,3 +79,1 @@`:** line 141 splits it explicitly. The `platforms` deletion and addition go to FIX-OX-08; the removed `push: true` and `tags:` go to FIX-OX-11. I checked that rebuilding forward from the base and reversing FIX-OX-11 from the checkpoint both leave the same order (push, tags, platforms), so FIX-OX-08's B tree has one well-defined expected SHA.
- **R21 revision row:** now present at line 9614.

## 5. Non-blocking P3 notes
1. **"clean-clone/source-diff" evidence is named but not defined (line 85).** Line 81 has no step that produces it. Suggest defining it as: the existing M0 clean-clone record, plus an unchanged source-hunk SHA for `ea0d3d5..<checkpoint-SHA>` re-derived after the amendment.
2. **No fetch before the ahead/no-divergence check (line 81).** A stale `origin/main` ref would report "not behind". Run `libra fetch origin main` before `status --short --branch`.
3. **No recovery path for an unsigned amendment.** Soft-reset recovery is limited to the original worktree (line 79). If the amendment lacks gpgsig, there is no defined fix, so the plan stops safely and needs another revision.
4. **Code, test or workflow files swept into the amendment would have no owner.** Coordinate mapping (line 82) only counts accepted predecessors. Any such file would make the A/B or replay SHA checks stop safely, but nothing says this up front. Suggest recording that amendment-time code, test and workflow paths must be empty, or else stop.
5. **`plan-status.md` is out of date** (lines 13, 49, 64, 88). It still says R24 PASS and that the checkpoint and clean clone are yet to happen, and it does not mention R25 FAIL or R26. README line 48 is current. Since `plan-status.md` goes into the amendment commit, sync it first, so nobody reads it as an instruction to rebuild the checkpoint.
6. **Write sets conflict with the no-write-back rule (pre-existing).** 71 cards list `docs/plan/plan-20260920.md`（本卡状态/证据）as writable, but line 79 forbids writing card status or evidence back into the plan file. The write sets only permit, never require, so the stricter rule wins. It would still be worth saying that status goes to `plan-status.md` and evidence goes under `evidence/`.

I found no P0, P1 or P2 findings. Lifecycle and status were treated as plan state, not proof that any code is done. I found no claims that go beyond the evidence, and no credentials exposed.

VERDICT: PASS
