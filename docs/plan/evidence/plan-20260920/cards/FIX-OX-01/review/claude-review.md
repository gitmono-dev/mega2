**FIX-OX-01 ER-05 review (Claude Code, read-only)**

I found no P0, P1 or P2 issues. There are five P3 notes, none of which blocks the card.

**Scope and hashes**
- **Changed lines are all owned by FIX-OX-01.** There are two owner sub-hunks:
  - Sub-hunk 04 is `.github/workflows/docker.yml:32-41`. This is the stable-tag guard plus the blank line after it, which the plan assigns to FIX-OX-01 (`plan-20260920.md:143,150`).
  - Sub-hunk 17 is `docker.yml:101-224`, the manifest job. The plan assigns it at `plan-20260920.md:146`.
- **The rest of each checkpoint hunk belongs to the accepted predecessors.** In checkpoint hunk `@@ -14,0 +32,20` (`source-u0.diff:29-49`), lines 42-51 are FIX-OX-08's. In hunk `@@ -50,0 +83,139` (`source-u0.diff:77`), lines 83-100 are FIX-OX-11's. The line counts add up: 10+10=20 and 18+121=139.
- **The only change after the checkpoint is the digest-validation fix.** It is the 1-removed / 4-added edit at `docker.yml:196` (`source-after-checkpoint.diff`). It sits inside sub-hunk 17, so the count goes from 121 to 124. `unstaged-source.diff` is identical to it.
- **The predecessor delta is empty.** `predecessor-source-diff` exits 0 with no output (its sha is the empty-file sha `e3b0c4…`).
- **The candidate hash matches everywhere.** Candidate `ecc44850…` is the same in `owner-coordinate-map.json`, `dry-run-results.json`, `apply-results.json`, `acceptance-simulation.json` and `verification-results.json`.
- **A removes only FIX-OX-01.**
  - The dry run reverses 17 then 04 with `patch -C -R -p1 -F0`. Both exit 0 and the output is only "patching file".
  - The apply reaches `c3b97f47…`, which is the A expected hash. Only `docker.yml` changed, and all six test-file hashes are unchanged.
  - The FIX-OX-08/11 build job (`docker.yml:17-99`) is kept.
- **B keeps the whole candidate.** It applies an empty reverse set and stays at `ecc44850…`. Its ` M docker.yml` status is just the uncommitted correction relative to `a5ba8feb`.
- **No other source, test or workflow path changed.**

**Acceptance criteria**
- **AC-1 met.** The manifest job has `needs: build`, `if: !cancelled()`, and its first step is `test "$BUILD_RESULT" = success` (`:102-112`), which runs before checkout. `fail-fast: true` means a failure in either architecture stops publication.
- **AC-2 met.**
  - `nullglob` plus an exact count of 2 (`:186-191`).
  - Each digest is checked against `^[0-9a-f]{64}$` with an explicit error and `exit 1` (`:196-199`).
- **AC-3 met.** The create step gets `type=ref,event=tag` and `semver {{version}}`, with `latest` and the minor tag filtered out (`:203-215`). The simulated docker log shows `--tag …:v1.2.3 --tag …:1.2.3`.
- **AC-4 met.** `imagetools inspect --raw` piped to `jq -e … sort == ["amd64","arm64"]` (`:216-217`). Provenance attestations (`unknown` os) are excluded by the linux filter. GitHub's bash runs with `-eo pipefail`, so a mismatch fails the step; the `wrong_manifest_architectures` case exits 1.
- **AC-5 and AC-6 met.** The minor and global latest checks use `max()` on parsed canonical tags, and a missing current tag stops the job (`:130-146`). All four channel simulations match their expected outputs.
- **AC-7 met.**
  - The bash regex at `:37` is checked by VER-3 with valid and invalid example tags.
  - It runs before Checkout (`:52`) and Login (`:55`).
  - The manifest job can only reach its login (`:149`) after the build jobs succeed, and it re-checks the tag in Python before login.

**Digest validation fix**
- On Bash 3.2.57, the old bare `[[ … ]]` let a non-hex digest through to `imagetools create` (`pre-fix-digest-validation-failure.json`, exit 0).
- The fixed version rejects malformed, short and uppercase digests with exit 1 and an empty docker log, so it fails closed before Docker is ever called.

**VER-1 (host workaround, not a pass)**
- The default actionlint 1.7.12 run timed out (exit 124, no output). It is correctly recorded as a non-pass.
- The schema-only 1.7.12 run and the full run with the patched upstream build (`v1.7.13-0.20260419144658-011a6d15e749`) both exit 1. Each reports only `107:7: unexpected key "queue"`.
- The exact plan fallback asserts the line occurs once and prints a one-line diff removing only `      queue: max`. It exits 0 with empty stderr.
- `actionlint-host-workaround.md:9` and `verification-results.json:9` say explicitly that this is not a pass of the original workflow.

**VER-2 and VER-3**
- Both exit 0 with empty stderr.
- VER-2's output lists `needs: build` (`:102`), `needs.build.result` (`:111`), `imagetools create`/`inspect` (`:215-223`) and the minor/latest lines. The structural concurrency check passed with no output.

**Credentials, history and release boundary**
- The simulations record `credentials_used: false` and `docker_network_or_registry_used: false`, and docker was stubbed.
- Historical tag v0.42.25 (target `4b645af…`, run `37704534619`) is still recorded as a Docker Build Cloud failure: "prepaid build minutes limit of 1200 reached". It is not presented as a current result.
- No version bump, push, tag or Release was made. The real tag build and Docker publication are left to OX-284 under D-OX-TAG.

**P3 notes (non-blocking)**
1. **`docker.yml:89` (FIX-OX-11's hunk, outside this card):** `Export digest` still uses a bare `[[ "$DIGEST" =~ … ]]`. It works on the Ubuntu runner's bash 5 with `-e`, but has the same Bash-3.2 weakness this card fixed. Suggested fix, in a FIX-OX-11-owned follow-up: use the same `if [[ ! … ]]; then echo …; exit 1; fi` form.
2. **`docker.yml:210`:** the check only requires at least one immutable tag. For a stronger AC-3 guarantee, require exactly two and check that both `"$REGISTRY_IMAGE:$REF_NAME"` and `"$REGISTRY_IMAGE:${REF_NAME#v}"` are present.
3. **`docker.yml:139,141-143,146`:** the `current is not None` checks are dead code after the early exit at `:135-136`. They could be removed later as cosmetic cleanup.
4. **Evidence traceability:**
   - `actionlint-original-diagnostics.*` (exit 1) and `actionlint-original-version.*` are not listed in `verification-results.json`, and the command that produced them is not recorded.
   - The fallback run does not say which actionlint binary was on PATH.
   - Fix: record the exact command line and binary version for each.
5. **`acceptance-simulation.json`:** the publish cases never set `PUBLISH_MINOR` or `PUBLISH_LATEST` to `true`, so `docker.yml:219-224` is untested locally. Adding one case with each set to `true` would cover it.

VERDICT: PASS
