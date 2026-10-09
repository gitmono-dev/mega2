I couldn't run your script. This session only has read-only Read/Glob/Grep tools: no shell and no file writing. So nothing was written to `round-1/claude.stdout`, `claude.stderr`, `claude.exit` or `metadata.json`. Instead I did the review that `prompt-1.md` asks for, directly against the packet. To get the files and `metadata.json`, run the script from a normal shell.

# FIX-OX-02 review (round 1)

Verdict: **FAIL**. Four blocking issues need fixing; the ones that matter most are how the B variant was built and a default-path change that AC-6 doesn't account for.

## What checks out
- **Order of construction (AC-1/2/3):** `context/source/snapshot_content_tests.rs` runs the steps in the right order:
  1. builds the fixture (719–904);
  2. seeds facts (905–936);
  3. publishes and certifies (937–954);
  4. installs faults (955–958);
  5. sends the HTTP resolve (965–980).
- **Size and digest:** both rooted tests supply size and digest that match their simulated stream (`snapshot_chunks_bounded_tests.rs:162–188`, `388–416`).
- **No mutation after certification:** after resolve, the only change is the in-memory `fixture.digest = digest` (line 208); certified facts aren't touched. `post-checkpoint-source.diff` is exactly that reordering.
- **AC-4/AC-5:** the batch test asserts `413/LIMIT_EXCEEDED` (`chunks:469–483`) and `404/PATH_NOT_FOUND` (`chunks:489–498`), each with zero body reads.
- **Exit files match the ledger:**
  - `final-VER-1/2`: 0, "1 passed".
  - `B-isolated-VER-1/2`: 0.
  - `A-full-baseline-VER-1/2`: 101, real 502-vs-200 and 502-vs-413 failures, not compile errors.
  - `A-red-post-VER-1/2`: 101, same failures.
  - `A-full-scope-VER-1`: compile error (E0422/E0599), correctly recorded as superseded.
  - `nightly-fmt-check`: 0.
- **A-full hashes:** they match base `ea0d3d5` for all three files (`A-full-base-hash-result.json`). Masking hunks 45–47 is explained by the compile errors in `A-full-scope-VER-1.stderr`.
- **Source binding (gate 3):** the manifest hashes equal the execution-tree hashes in `variant-file-hashes.json:23–27`. This card's delta touches only `snapshot_content_tests.rs`; `snapshot_objects_bounded_tests.rs` appears only as context.
- **Release boundary (gate 5):** no version bump, push, tag, or done/complete claim. The README release-boundary section (line 30) and the card's `Lifecycle=pending` both hold that line.

## P1 findings (must fix before PASS)
1. **B may not contain this card's own correction.**
   - `dry-run-results.json:2` and `apply-results.json:2` record B's starting point as parent HEAD `30ade79`.
   - Patch `33-FIX-OX-02.patch` (`@@ -875,0 +922,37`) only fits the parent-HEAD layout (facts after publication). That confirms the A variants were built from parent HEAD.
   - Nothing records ordinals 49–52 (the post-checkpoint reordering) being applied to B. B's hash `6e381c4…` is not tied to the candidate tree.
   - So the "exact B" green results may reflect the old, AC-3-violating order.
   - **Fix:** rebuild B from `30ade79` plus `post-checkpoint-source.diff`, reverse 35–38, record receipts and hashes, and rerun B-isolated VER-1/2 with B's own target directory.
2. **AC-6 default path changed without evidence.**
   - Before, the resolve request had no `lease_seconds` field (`source-u0.diff:233`). Now every default fixture sends `lease_seconds: 600` (`snapshot_content_tests.rs:974`).
   - Nothing in the packet shows the server default is 600, so "other tests keep their default path" is unsupported.
   - **Fix:** send the field only when `Some`, or cite evidence that the server default is 600.
3. **Two numbering schemes are both called "ordinal".**
   - `owner-coordinate-map.json` maps raw hunks 21–38 to owner ordinals 25–42.
   - But `dry-run-results.json`, `apply-results.json`, `A-red-variants.json`, the patch filenames, and `verification-results.json:28,37` ("owner ordinals 21-28") use the raw numbers.
   - README:24 then uses the owner numbering for "45-47".
   - Anyone reproducing from the map would reverse the wrong hunks.
   - **Fix:** use one name for each scheme everywhere.
4. **A required input is missing.** `context/task-card-and-owner-table.md` is listed in the prompt but isn't in the packet. **Fix:** add it.

## P2 residuals
- **Malformed patch tails.** `owner-patches/28-FIX-OX-02.patch:6–9` and `38-FIX-OX-06.patch:10–13` end with empty `diff --git` headers for other files. They still applied with exit 0, but should be stripped and re-hashed.
- **Raw logs missing from the ledger.**
  - `verification/B-VER-1` (exit 101, 502), `B-VER-2`, `B-repro-VER-1`, and `A-red-VER-1/2` aren't listed in `verification-results.json`.
  - The B runs show `A-red/target` in their stderr, which fits the excluded shared-target diagnostic. But the exclusion isn't tied to these specific files.
  - The ledger id `A-red-VER-1` actually points to the `A-red-post-*` files.
  - **Fix:** list each one explicitly as an excluded diagnostic, with the reason.
- **Shared blob fact.** The `/file` fact rewrites the record for a blob that `/alias`, `/executable`, `/nested/file` and `outside` also use. This only affects the named fixture, so it's acceptable as residual risk.

VERDICT: FAIL

The plan-mode wrap-up steps (writing the plan file and calling ExitPlanMode) also weren't possible here, because neither tool is available in this session.
