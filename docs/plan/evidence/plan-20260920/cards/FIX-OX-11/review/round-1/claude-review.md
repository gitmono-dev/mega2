# FIX-OX-11 ER-05 review (Claude Code, read-only)

**Scope:** I reviewed the snapshot under `docs/plan/evidence/plan-20260920/cards/FIX-OX-11/review/snapshot/` using only Read, Glob and Grep. I didn't run anything, so I couldn't recompute any SHA-256 myself. Hash checks below are cross-file consistency checks, plus my own manual replay of the hunks against `comparison/*.yml`. I reviewed local card acceptance only. Nothing here implies a push, tag, version bump, Release or Docker publication; OX-284 is still the only release gate.

## Checks

**1. Accepted-predecessor mapping**
- `source-diff-integrity.json:73-95` shows that `libra diff --name-only 81f2e5c → 346fe3c` over all seven paths exits 0 with empty output (`"empty": true`).
- `coordinate-map-and-expected-hashes.json:358-360` gives delta 0 and identity mapping.
- The FIX-OX-08 block line deltas (`:286-356`) agree with the manifest's old and new counts. Examples: ordinal 3 is 2→14 (+12), ordinal 10 is 3→0 (−3), ordinal 12 is 1→0 (−1).

**2. A set (10 hunks, FIX-OX-11 + FIX-OX-01)**
- The order is 17, 16, 15, 13, 11, 9, 8, 7, 6, 4. The canonical starts are 101 > 83 > 81 > 78 > 74 > 69 > 67 > 65 > 61 > 32, which is strictly descending. The 78 tie with ordinal 12 doesn't matter because 12 is FIX-OX-08 and not in this set.
- The Apple `new_start+1` adjustment is applied, after mapping, to exactly the `new_count=0` hunks: 8 (67→68), 9 (69→70) and 13 (78→79).
- The patch files confirm it: `owner-patches/08…:3` has `+68,0`, while `canonical-owner-subhunks/08…:3` has `+67,0`.

**3. B set (2 hunks, FIX-OX-01)**
- The order is 17, then 4 (`apply-results.json:156-159`).

**4. Dry-run results** (`dry-run-results.json`)
- The command is `patch -C -R -p1 -F0`.
- Before and after hashes are both `3a977717…7948712` for A and for B.
- Every exit code is 0.
- stdout contains only `patching file`, stderr is empty, and `forbidden_diagnostics` is 0.

**5. Apply results** (`apply-results.json`)
- A: actual = expected = `6441158f…35c99b3`.
- B: actual = expected = `c3b97f47…afab0eae`.
- These match the coordinate map at `:4-5` and the manifest's `owner08_only_forward_replay_sha256` at `:29`.
- In both copies `changed_paths` is only `.github/workflows/docker.yml`, and the six `src/api/router/*_tests.rs` hashes are identical across A-dry, B-dry, A-apply and B-apply.

**6. Manual replay**
- Reversing the A set from `checkpoint-docker.yml` gives exactly `A-docker.yml`. In particular, hunk 13 inserts `push: true` and `tags:` after `context: .` (A:68-74), and hunks 8 and 9 restore `type=raw,value=latest` and `{{major}}.{{minor}}` around lines 56-60.
- Reversing 17 and then 4 gives exactly `B-docker.yml`.
- B keeps all FIX-OX-08 owner lines and all FIX-OX-11 owner lines. It has none of the FIX-OX-01 lines (no stable-tag guard, no docker job).

**7. Ownership**
- Ordinal 13 (`push: true`/`tags:`) belongs to FIX-OX-11, and ordinals 12 and 14 (the platform selector) belong to FIX-OX-08. This matches `plan-20260920.md:142`.
- The blank lines at checkpoint lines 83 and 100 are covered by hunk 16. This matches `plan-20260920.md:152-153`.

**8. Acceptance criteria against B (`comparison/B-docker.yml`)**
- **AC-1 / AC-2:** each matrix row builds only `platforms: ${{ matrix.platform }}` (line 69). It pushes by digest with no tags (line 71), and the native-arch guard is at lines 32-35. ✓
- **AC-3 / AC-4:** `[[ "$DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]]` runs under explicit `shell: bash`, so a mismatch fails the step (lines 75-79). ✓
- **AC-5 / AC-6 / AC-7:** artifacts are uploaded as `digests-${{ matrix.slug }}`, which gives the distinct names `digests-amd64` and `digests-arm64`, with `if-no-files-found: error` (lines 83-88). ✓
- **AC-8:** `provenance: true` is at line 72. The metadata step "Extract image labels" sets `flavor: latest=false` and only immutable ref/semver patterns. Only `outputs.labels` is used (line 70), and there is no `tags:` or `push: true` input. ✓

**9. VER-1 and VER-2** (`verification-results.json`)
- VER-1 exits 0 and all nine checks are `True`. Each pattern holds in B:
  - the exact `(platform, runner, arch, slug)` tuples (lines 22-29)
  - one `platforms:` template
  - one `push-by-digest=true`
  - the regex
  - one artifact name
  - `if-no-files-found: error`
  - one `provenance: true`
  - the labels step
  - `latest=false`
- VER-2: actionlint 1.7.12 exits 0 with no diagnostics on the original file. That is consistent with B, which has no `queue: max` line, so the syntax-only fallback correctly wasn't used.
- These static checks only cover platform, runner, arch and slug. Real tag-time behaviour remains OX-284's D-OX-TAG.

## Findings

**P0 / P1 / P2:** none.

**P3-1 — VER results aren't explicitly tied to the B copy** (`verification-results.json:4-20`)
- The record has no copy name, working directory, or `docker.yml` SHA, so readers have to infer that the run used B. The inference is sound:
  - VER-1 would fail on A.
  - Per the plan (`plan-card-context.md:18`), actionlint 1.7.12 flags `queue: max` in HEAD/checkpoint, so its clean exit rules those out.
- Fix: add `"copy": "B-apply"` and `"docker_sha256": "c3b97f47…"` to each VER entry, so the batch AC-5 rule ("evidence bound to the reviewed diff", `plan-20260920.md:6072`) can be checked directly.

**P3-2 — B ends in a trailing blank line** (`B-docker.yml:90`)
- This is intended: the plan assigns checkpoint line 100 to FIX-OX-11 (`plan-20260920.md:153`), and the line disappears in the final tree once FIX-OX-01 lands. It only matters if a yamllint `empty-lines` check is ever run on this intermediate state. No change needed.

**P3-3 — The M0 manifest's mapping notes are out of date** (`owner-hunk-manifest.json:493-494`)
- `accepted_predecessor_mappings: []` and the "no card predecessor has been accepted" note describe the M0 state.
- The card-level `coordinate-map-and-expected-hashes.json:281-361` supersedes them and is correct. Optionally add a pointer from the manifest to the card-level map.

VERDICT: PASS
