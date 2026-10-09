# Claude Code review of plan-20260920, R28 snapshot (SHA `ff3c36e9…1ddf`)

This was a read-only review using only Read, Glob and Grep. I made no edits and executed no cards. The plan has no P0, P1 or P2 findings; I found four P3 notes, none of which block the gate.

## Closure of the six R27 P3 findings

| R27 P3 | Where it is fixed in R28 | Status |
|---|---|---|
| A fixed commit range can't catch new source deltas in an amendment | M0 checklist line 81: `libra diff --name-only --old <checkpoint> --new <amendment>` over the source/test/workflow paths must be empty; if not, a `--unified=0` diff is saved and every changed line mapped to one owner. `m0-source-diff-post-amendment.json` records `changed_paths: []` for `9a6d705`. | Closed |
| Path-level owner checks don't prove one owner per hunk | Line 81 ("路径级映射不能代替 hunk 级归属", i.e. path mapping can't stand in for hunk ownership), line 83 (each `+/-` line gets one owner) and line 199 (an unmapped hunk blocks the commit) | Closed |
| Whole-tree commits clashed with the REL-OX-01 one-card-per-commit rule | Line 83 (end) and line 199: commit every current non-credential uncommitted path, but only the named target card moves Lifecycle/Acceptance; every other owner stays pending | Closed |
| No status-sync step after a PASS | Lines 79, 82 and 83: after each PASS, README and plan-status are synced first, then `libra status --short` is captured before staging | Closed |
| FIX-OX-04 still called its tree a clean copy | GAP-OX-01, line 85, and the FIX-OX-04 Description and AC-4: the baseline is the checkpoint-inclusive copy, and "WIP-free baseline" is explicitly forbidden as a label for it. A WIP-free run is only an optional read-only diagnosis on a detached `ea0d3d5` copy. | Closed |
| The raw helper didn't check the sign-off identity or say it skips crypto verification | `verify-raw-commit-headers.py:32-41` parses the last paragraph of the message, requires the committer email to appear in a `Signed-off-by` trailer, and requires a `gpgsig` header. Line 79 states "它不执行密码学验签" (it does not do cryptographic signature verification). | Closed |

## Other checks

- **All 306 cards pending:** 306 `**Lifecycle / Acceptance:**` lines, and all 306 read exactly `` `pending` / 空 `` (empty). The only "done" mentions are rules (lines 173, 9574, 9582), not card states.
- **Ordering and owners:**
  - The serial chain (line 174) starts FIX-OX-08 → 11 → 01 → 02 → 09 → 04, which matches the gate on line 85.
  - REL-OX-01 lists 195 + 108 = 303 members, plus OX-284, OX-283 and OX-20, for 306.
  - The Docker owner table covers the split of the `-45,3 +79,1` hunk and the blank lines 8, 12, 15, 41, 51, 83 and 100.
  - Each A/B subset for FIX-OX-08, FIX-OX-11 and FIX-OX-01 is spelled out.
- **FIX-OX-08 and Apple `patch -R`:** canonical and application patches have separate headers and SHAs. The sequence is: map to current coordinates, sort by the mapped canonical coordinate in descending order, then apply the `new_start+1` shift only when `new_count=0`. The shifted value is never used for sorting. A/B checks use exact-byte hashes, and the negative control must fail on the hash even when the exit code is 0. The full forward replay uses canonical edits and rebuilds files byte-for-byte, including files with no newline at end of file.
- **Earlier findings:**
  - R24: the concurrency-block regex is in FIX-OX-01 VER-2, the dual-owner hunk split is at line 142, and R21 is in the history.
  - R25: the restart gate moved to an amendment commit with the checkpoint kept; ordering and fixtures are on line 83; R24 and R25 rows are in the log.
  - R26: the source-diff evidence definition and fetch-before-ahead check are on lines 81–82; unsigned-commit recovery is on lines 79 and 82; the no-write-back override is on line 79.
- **Recovery is runnable:** a failed signature check leads to `reset --soft HEAD^`, a byte-for-byte check of the index, and a retry. If the retry fails, the change stays staged and work stops.
- **Release boundary:**
  - No version bump, push, tag, Release or versioned Docker publish is allowed before OX-284 (lines 117, 191, 197 and 205).
  - OX-12's dependency change to `Cargo.lock` is explicitly not a version bump.
  - OX-284 makes the single patch bump.
  - OX-20 makes no bump and has no D gate.

## P3 findings (non-blocking)

1. **The original checkpoint and the R27 amendment were checked with the old helper:** `m0-source-diff-post-amendment.json:61` records only "Signed-off-by=true", from before the R28 trailer-identity check existed.
   - **Where:** M0 line 84 and line 86.
   - **Fix:** require re-running the R28 helper on `81f2e5c` and `9a6d705`, and save the output, before restarting FIX-OX-08 A/B.
2. **Line 81 says the checkpoint→amendment diff "must be empty", then describes what to do if it isn't.** Once later cards commit corrections, that diff won't be empty.
   - **Fix:** reword it as "empty, or every changed line maps to an accepted-predecessor or owner-manifest entry".
3. **The status ledgers are out of date:** `plan-status.md:161` still says the R27 amendment commit is pending, but plan line 56 says `9a6d705` is already committed. The new sync-after-PASS rule will fix this; just confirm it is done in the R28 amendment commit.
4. **Leftover write-back wording:** FIX-OX-04 Deliverables and the card write sets still name `docs/plan/plan-20260920.md` for status and evidence. Line 79 overrides this, so it isn't blocking; for clarity, point those at `plan-status.md` and `evidence/plan-20260920/` instead.

The plan doesn't need a Codex isolated audit, per your current gate. Separately, the claude.ai Google Calendar connector needs to be authorized in your claude.ai connector settings before it can be used; it wasn't needed for this review.

VERDICT: PASS
