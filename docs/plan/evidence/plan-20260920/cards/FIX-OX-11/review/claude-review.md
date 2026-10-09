I read every artifact in the snapshot using only Read, Glob and Grep. I didn't change any file, status or card. I found no P0, P1 or P2 issues, only two non-blocking P3 notes.

### Predecessor mapping
- **Empty post-checkpoint diff:** the recorded diff from checkpoint `81f2e5c` to HEAD `346fe3c` covers the seven source/test/workflow paths. It printed nothing, and the record marks it `empty: true` (`source-diff-integrity.json:73-95`).
- **No line shift applies:** for every accepted FIX-OX-08 block, the current start equals the checkpoint start (`coordinate-map-and-expected-hashes.json:281-360`, `post_checkpoint_source_test_workflow_delta: 0`). So the canonical coordinates map unchanged.

### A/B hunk sets and order
- **A set (10 hunks):** order 17(101), 16(83), 15(81), 13(78), 11(74), 9(69), 8(67), 7(65), 6(61), 4(32). That is descending by canonical start. The only shared start is 78, and ordinal 12 at 78 is FIX-OX-08, which A keeps, so no tie-break is needed.
- **B set (2 hunks):** order 17, then 4 (`apply-results.json:253-256`).
- **Accepted hunks excluded:** ordinals 1, 2, 3, 5, 10, 12 and 14 (FIX-OX-08) appear in neither set.
- **Apple `+1` adjustment:** it appears only on the `new_count=0` application headers: 08 (`+68,0`), 09 (`+70,0`), 13 (`+79,0`), plus the unused 10 (`+73,0`) and 12 (`+79,0`). The canonical sub-hunks keep 67, 69, 78, 72 and 78. Every other header is identical between `owner-patches/` and `canonical-owner-subhunks/`. All of this matches the manifest at `owner-hunk-manifest.json:30` and lines 179-274.

### Dry-run and apply
- **Dry-runs:** for both A-dry and B-dry, the before and after hashes are both `3a977717…` (the checkpoint). Every exit is 0, `forbidden_diagnostics` is 0, and stdout is only `patching file …` with empty stderr (`dry-run-results.json:17-18,168-169`).
- **Applies:** both hashes match the independent expectations.
  - A: `6441158f…99b3`, actual equals expected (`apply-results.json:7-8`), and it matches the manifest's FIX-OX-08-only replay (`owner-hunk-manifest.json:29`).
  - B: `c3b97f47…0eae`, actual equals expected (`apply-results.json:250-251`).
- **Argv and working directory:** each applied hunk records the exact argv `patch -R -p1 -F0 -i <abs patch>` and its directory (`/tmp/mega2-fix-ox-11-A-apply` or `/tmp/mega2-fix-ox-11-B-apply`).
- **Changed paths:** `.github/workflows/docker.yml` is the only changed path. The six `src/api/router/snapshot_*_tests.rs` hashes are identical across A-dry, B-dry, A-apply and B-apply, with `non_target_source_paths_match: true`.

### Content cross-check
- **B workflow:** `comparison/B-docker.yml` equals the checkpoint minus ordinal 4 (lines 32-41) and ordinal 17 (lines 101-221). Checkpoint lines 42-100 map line-for-line onto B lines 32-90.
- **A workflow:** `comparison/A-docker.yml` equals B with ordinals 6, 7, 8, 9, 11, 13, 15 and 16 reversed. FIX-OX-08's matrix, Cloud driver removal (ordinal 10) and the platform-selector swap (ordinals 12 and 14) remain. Its order of push, tags, `platforms: ${{ matrix.platform }}` and labels is consistent with the base file.

### Acceptance criteria (B state)
- **AC-1/AC-2 (each row pushes only its own digest):** the build step uses `platforms: ${{ matrix.platform }}` (`B-docker.yml:69`) with `push-by-digest=true,push=true` and no `tags:` input (`:71`).
- **AC-3/AC-4 (`sha256:<64hex>` format):** the guard is `[[ "$DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]]` (`:79`).
- **AC-5/AC-6/AC-7 (distinct per-platform artifacts):** the upload uses `digests-${{ matrix.slug }}`, giving `digests-amd64` and `digests-arm64`, with `if-no-files-found: error` (`:86-88`).
- **AC-8 (provenance and metadata step):**
  - `provenance: true` (`:72`).
  - The step is named "Extract image labels" and sets `flavor: latest=false`.
  - It lists only immutable tag patterns, and those tags are not passed to the build (`:51-59`). The build consumes only the labels.

### Verification results
- **VER-1:** the command is byte-exact to the card (`plan-card-context.md:17`). It exited 0 and printed all nine checks `True`. It is bound to copy `B-apply`, SHA `c3b97f47…`, HEAD `346fe3c` (`verification-results.json:5-14`).
- **VER-2:** actionlint 1.7.12 exited 0 on the original B-state file, with empty stderr. B has no `queue: max` line, so the syntax-only fallback correctly did not run. It is bound to the same copy and SHA (`:17-25`).
- **Remote behaviour not established:** both are static checks. Tag-time two-architecture behaviour is still decided by OX-284 D.

### P3 closure
`P3-closure.md` binds both VER results to B-apply and its SHA. It also records the accepted notes: B's trailing blank line, the M0 mapping note and the VER wording. It shows the apply argv and working directories as added.

### Findings
- **P0 / P1 / P2:** none.
- **P3-1 (dry-run argv):** the dry-run records keep only a shared prefix, `patch -C -R -p1 -F0`, with no `-i <file>` and no working directory per hunk (`dry-run-results.json:10-16,161-167`). Apply has both.
  - Fix (optional): add the per-hunk `command` and `working_directory` fields to the dry-run results, matching `apply-results.json`. This doesn't affect acceptance, because the patch hashes and the unchanged before/after hashes already identify the inputs.
- **P3-2 (plan hash):** `owner-hunk-manifest.json:6` carries the M0-time plan SHA `ff3c36e9…`, while the card evidence uses `c28381c8…`. This is the historical M0 mapping already accepted as Round 1 P3-3. No action.

This covers local card acceptance only. Nothing here implies a push, tag, version bump, Release or Docker publication; OX-284 remains the only final release gate.

VERDICT: PASS
