# FIX-OX-08 ER-05 review (R28, plan SHA `ff3c36e9…bfdfbc1ddf`)

I found no P0, P1 or P2 issues. The root `.github/workflows/docker.yml` is the correct B state: only the FIX-OX-08 changes are present. I used Read, Glob and Grep only, so I could not recompute any SHA-256. I checked that the hashes agree across the manifest and the result files, and I rebuilt the B file by hand.

## 1. Owner split and coordinates (`owner-hunk-manifest.json`)

I checked all 17 canonical sub-hunks line by line against `comparison/base-docker.yml` and `comparison/checkpoint-docker.yml`.

**Insertions and replacements land on the right checkpoint lines:**

| Ordinal | Header | Checkpoint lines |
|---|---|---|
| 1 | `-7,0 +8,1` | 8 |
| 2 | `-10,0 +12,4` | 12–15 |
| 3 | `-12,2 +17,14` | 17–30 |
| 4 | `-14,0 +32,10` | 32–41 |
| 5 | `-14,0 +42,10` | 42–51 |
| 6 | `-24,1 +61,1` | 61 |
| 7 | `-28,1 +65,2` | 65–66 |
| 11 | `-41,1 +74,2` | 74–75 |
| 14 | `-47,0 +79,1` | 79 |
| 15 | `-49,1 +81,1` | 81 |
| 16 | `-50,0 +83,18` | 83–100 |
| 17 | `-50,0 +101,121` | 101–221 |

**Deletions use the "line before" convention**, which matches the Libra fixture (`@@ -2,1 +1,0 @@`):
- 8 → after checkpoint 67 (`tags: |`)
- 9 → after 69
- 10 → after 72 (`setup-buildx`)
- 12 and 13 → both after 78 (`context: .`)

**Line totals match.** There are 14 removed and 185 added lines, 199 in total, which matches `source_change_line_count`. The checkpoint length works out as 50 − 14 + 185 = 221, which is correct.

**Blank-line owners match the card table:**
- Lines 8, 12, 15 and 51 → FIX-OX-08.
- Line 41 → FIX-OX-01.
- Lines 83 and 100 → FIX-OX-11.

**The original `@@ -45,3 +79,1 @@` hunk is split correctly:**
- Ordinal 12 (delete the old `platforms`) → FIX-OX-08.
- Ordinal 13 (delete `push`/`tags`) → FIX-OX-11.
- Ordinal 14 (add `platforms: ${{ matrix.platform }}`) → FIX-OX-08.

No later digest, artifact or manifest change is attributed to FIX-OX-08.

## 2. Apple patch transform and reverse order

- **Transform:** only the five `new_count=0` patches (8, 9, 10, 12, 13) have `new_start+1` in `apple-application-subhunks/`. All other headers and SHAs are byte-equal to the canonical ones (Grep of every `@@` line).
- **Order:** both A runs use 17,16,15,14,13,12,…,1. Ordinals 13 and 12 tie at canonical 78 and are broken by ordinal, descending. Ordinal 5 (line 42) correctly runs before 4 (line 32). B uses the same order with 08 left out.
- **Ordering check:** I traced 14 → 13 → 12 and they rebuild base lines 45–47 in the right order. In B, reversing 13 gives `context`, `push`, `tags`, `platforms` (root lines 68–71), which matches the forward replay "insert after old 47".
- **Preflight:** the R26 Apple preflight has 13 cases, including the adjacent-owner split and a negative control that exits 0 but restores the wrong bytes. The Libra fixture confirms the four zero-count coordinates.

## 3. A/B results

- **A-dry and B-dry:** every exit is 0 with empty stderr and stdout `patching file …`. There are no forbidden diagnostics; Grep for offset, fuzz, reject, FAILED and hunk found nothing in `raw-results/`.
- **A-apply** (17 patches): `5f5af640…472b` equals the base SHA in the manifest.
- **B-apply** (10 patches): `6441158f…99b3` equals `owner08_only_forward_replay_sha256`.
- **Manual rebuild:** base plus the seven 08 edits reproduces the root workflow line for line (lines 1–74).
- **Full canonical replay:** recorded as `3a977717…7948712`, equal to the checkpoint.
- **Other paths:** `changed_paths` lists only the workflow.
- **`.env.test`:** a regular file, not a symlink, mode `0600`, in both A and B.
- **Six non-target test paths:** byte-identical to the checkpoint in A-dry, B-dry, A-apply and B-apply; `all_match: true` in `non-target-source-verification.json`.

## 4. Acceptance criteria (AC) on the root workflow

- **AC-1:** `.github/workflows/docker.yml:22-23` uses `linux/amd64` with `ubuntu-24.04`. ✓
- **AC-2:** `:26-27` uses `linux/arm64` with `ubuntu-24.04-arm`. ✓
- **AC-3:** `:30` sets `runs-on: ${{ matrix.runner }}`. ✓
- **AC-4:** `:32-35` runs `test "$RUNNER_ARCH" = "$EXPECTED_ARCH"`, which exits non-zero on a mismatch. The arch values X64/ARM64 match. ✓
- **AC-5:** `:71` is the only `platforms:` line and uses `${{ matrix.platform }}`. ✓
- **AC-6:** `genedna/mono` and `driver: cloud` are gone (`:62-63`). ✓
- **AC-7:** `:37-40` removes exactly the four listed directories, then runs `df -h /`, before `Checkout` at `:42`. It does not touch the workspace or any variable path. ✓

## 5. Verification checks (VER)

- **VER-1:** exit 0.
- **VER-2:** actionlint 1.7.12 exits 0 with no diagnostics on the original file. The `queue: max` fallback was not needed because that line belongs to FIX-OX-01 and is absent here.
- **VER-3:** exit 0. The regex captures exactly lines 39–40 in the right order.

## Findings

No P0, P1 or P2. The P3 items below are non-blocking.

- **P3-1 – earlier VER-3 failure.** `raw-results/VER/VER-3.*` keep an earlier over-escaped attempt (`run: \\|`) that exited 1. `verification-results.json:40` discloses it and VER-3-final passes.
  - Fix: in the acceptance record, cite only VER-*-final as acceptance evidence and label VER-3 as a tooling misfire.
- **P3-2 – source diff context setting.** The manifest's `source_diff_command` uses `--unified=0`, but plan line 83 says to generate the full patch with `--unified=3` and then split it. The sub-hunks are the same either way.
  - Fix: record the `--unified=3` diff SHA too, or note why the two are equivalent.
- **P3-3 – no explicit predecessor mapping.** The manifest has no field saying "no accepted predecessors, identity mapping", even though plan line 83 requires the mapping.
  - Fix: add `accepted_predecessor_mappings: []`.
- **P3-4 – dry runs don't prove the file was untouched.** The dry-run JSONs record no before/after file SHA showing `-C` left the file unchanged, and the A/B results don't record `patch --version`.
  - Fix: add both fields in future runs. The apply hashes already close the correctness question.
- **P3-5 – R28 commit evidence not in this package.** The package does not include the R28 amendment-commit evidence: the raw signed-header check, `m0-source-diff-post-amendment.json`, and the link from `execution_head 0d10dad…` to `ff3c36e9…`. Plan line 86 also still names "R27".
  - Fix: cite those artifacts in the card's acceptance evidence.
- **P3-6 – intermediate B state (informational).** In the B-only state each matrix row would push the same mutable tags with `outputs: type=registry`. This is intended and is replaced by FIX-OX-11/01. The local commit holds the full checkpoint, and nothing is published before OX-284.

Only local card acceptance is in scope. I am not recommending any push, tag, version bump, Release or Docker publication.

VERDICT: PASS
