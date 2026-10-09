# FIX-OX-11 ER-05 review (Claude Code, read-only)

I found no P0, P1 or P2 issues. FIX-OX-11's local A/B evidence is internally consistent, matches the plan card at `plan-20260920.md:4616-4642` and lines up with the owner manifest. This review covers local card acceptance only. Static assertions do not show how the workflow behaves at tag time; that is decided by OX-284's D gate.

## Predecessor mapping
- The post-checkpoint check (`libra diff --name-only` from `81f2e5c` to `346fe3c`) covered all seven source/test/workflow paths. It exited 0 with empty stdout and `empty: true` (`source-diff-integrity.json:73-95`). So the identity mapping is justified (`coordinate-map-and-expected-hashes.json:6,358-360`).
- Each of the 7 FIX-OX-08 blocks has `checkpoint_new_start == current_copy_new_start` (`coordinate-map…json:286-356`).

## Hunk sets, ordering and the Apple `+1` adjustment
- **A set (10 hunks):** FIX-OX-11 ordinals {6, 7, 8, 9, 11, 13, 15, 16} plus FIX-OX-01 ordinals {4, 17}. They were applied in descending mapped coordinate order: 101, 83, 81, 78, 74, 69, 67, 65, 61, 32. That is order 17, 16, 15, 13, 11, 9, 8, 7, 6, 4 (`dry-run-results.json:20-31`). No two hunks in this set share a coordinate, so the ordinal tie-break is never used. Hunks 12 and 13 share coordinate 78, but 12 is FIX-OX-08 and correctly excluded.
- **B set (2 hunks):** order 17, 4 (`dry-run-results.json:171-174`).
- **Apple `+1`:** it applies only to the hunks with `new_count=0` (ordinals 8, 9 and 13), moving them 67→68, 69→70 and 78→79. In each case the application patch differs from the canonical sub-hunk only in the `+N,0` header; the bodies are identical:
  - `owner-patches/08-FIX-OX-11.patch:3` vs `canonical-owner-subhunks/08-FIX-OX-11.patch:3`
  - the same holds for files 09 and 13
  - The owner manifest's `application_new_start` values agree (`owner-hunk-manifest.json:192,257`).
  - All non-zero-count hunks keep identical canonical and application hashes.

## Dry-run and apply results
- **Dry-run:** `patch -C -R -p1 -F0` was run with Apple patch 2.0-12u11. Before and after hashes are both `3a977717…`, for A and for B. Every exit code is 0, stderr is empty, stdout is only `patching file …`, and `forbidden_diagnostics` is 0.
- **Apply:** the actual hashes equal the expected ones:
  - A: `6441158f…99b3` (`apply-results.json:7-8`)
  - B: `c3b97f47…0eae` (`apply-results.json:153-154`)
- **Changed paths:** only `.github/workflows/docker.yml`. The six other `src/api/router/snapshot_*_tests.rs` hashes are the same in all four runs.
- **Content check by reading the files:**
  - `comparison/A-docker.yml` is the checkpoint with the 10 hunks reversed. It still has FIX-OX-08's native matrix and arch check, and the pre-FIX-OX-11 metadata, push and outputs.
  - `comparison/B-docker.yml` is the checkpoint with only FIX-OX-01 lines 32-41 and 101-221 removed.

## Acceptance criteria (against `comparison/B-docker.yml`)
| AC | Evidence | Result |
|---|---|---|
| AC-1/2 | `platforms: ${{ matrix.platform }}` at :69 and `push-by-digest=true` at :71, run once per matrix row (amd64 at :22-25, arm64 at :26-29) | ✓ |
| AC-3/4 | `[[ "$DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]]` at :79 checks each row's own digest | ✓ |
| AC-5/6 | `name: digests-${{ matrix.slug }}` at :86, with `if-no-files-found: error` at :88 | ✓ |
| AC-7 | The two artifact names are different (`digests-amd64` and `digests-arm64`), so each platform can be fetched exactly | ✓ |
| AC-8 | `provenance: true` at :72. The metadata step at :51-59 has `flavor: latest=false` and only immutable `ref`/`{{version}}` tags. The build step at :64-72 has no `tags:` and no `push: true` (removed by hunk 13), and uses only `labels` | ✓ |

## VER-1 and VER-2
- **VER-1:** I evaluated all nine assertions by hand against B and each one holds.
  - The `matrix_rows` regex binds platform, runner, architecture and slug together, so swapping a slug would fail it.
  - Each of the count assertions (`provenance`, `push-by-digest`, `platforms:` and the artifact name) has exactly one match in B.
  - The recorded stdout shows all nine as True, with exit 0 (`verification-results.json:7-11`).
- **VER-2:** actionlint 1.7.12 on the original B workflow exited 0 with no diagnostics. B has no `concurrency.queue: max` line because that belongs to FIX-OX-01, so the syntax-only copy fallback was correctly not used. `original_file_checked: true`.
- **Binding:** both records name `copy=B-apply`, workflow SHA `c3b97f47…0eae` and HEAD `346fe3c` (`verification-results.json:12-14,23-25`).

## P3 closure
`P3-closure.md:5-7` binds both VER results to B-apply and its SHA. It also records the two accepted non-blocking notes accurately:
- B's trailing blank line is an intentional intermediate state. Hunk 16's final `+` blank line is at `owner-patches/16-FIX-OX-11.patch:21`, and FIX-OX-01's removal resolves it.
- The M0-time mapping note is historical.

## Findings
- **P0, P1, P2:** none.
- **P3-a:** `apply-results.json` records each patch's exit code and output but not the apply `command` array, while the dry-run file does (`dry-run-results.json:10-16`). Fix: add a `"command"` field (the presumed `patch -R -p1 -F0`) to each apply copy so the flags can be audited directly.
- **P3-b:** VER-1's wording says each assertion prints "PASS/FAIL" (`plan-20260920.md:4632`), but the command prints a Python dict of `True`/`False` (`verification-results.json:8`). The meaning is the same. Fix (optional): make the card's wording match the actual output in a later plan-text pass. This does not block FIX-OX-11.

Neither P3 affects the outcome. Nothing here recommends or implies a push, tag, version bump, Release or Docker publication; OX-284 is the only final release gate.

VERDICT: PASS
