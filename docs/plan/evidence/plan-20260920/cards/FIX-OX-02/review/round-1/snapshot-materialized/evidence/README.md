# FIX-OX-02 evidence

State: local candidate; focused A/B, acceptance, and formatting checks are recorded; Claude Code ER-05 review is still required before local acceptance and the authorized scoped submission.

## Scope and source history

The M0 checkpoint-owned fixture changes were already present in the execution HEAD after the prior whole-WIP local submission. This card validates their ownership and behavior and adds a post-checkpoint correction in `src/api/router/snapshot_content_tests.rs`: seed synthetic verified facts before native publication performs rooted certification. The accepted predecessors are FIX-OX-08, FIX-OX-11, and FIX-OX-01. The frozen plan remains SHA-256 `46d83eb6cf655ddbb96f80f960878c825691954e91b8c6f1515032768dbd9d74`.

The checkpoint source delta for the two owned test paths is `source-u0.diff`; the exact new delta from parent HEAD `30ade799cba59ed45b67504cbe68f20b68b81f92` is `post-checkpoint-source.diff`. Hunk ownership and canonical/application coordinates are recorded in `owner-coordinate-map.json`, `post-checkpoint-owner-map.json`, and the `*-owner-subhunks/` directories. The base file hashes, A/B hashes, and current execution-tree hashes are in `variant-file-hashes.json`.

## Acceptance evidence

- AC-1/AC-2: each large synthetic rooted fixture supplies size and SHA-256 matching its simulated chunk stream before native publication and rooted resolution.
- AC-3: the certified database fact is no longer edited after publication/resolution; the later in-memory `fixture.digest` assignment only selects the expected request digest.
- AC-4/AC-5: the batch fixture preserves `413/LIMIT_EXCEEDED` and `404/PATH_NOT_FOUND`; both assertions also require zero body reads before rejection.
- AC-6: empty/default options leave non-synthetic fixtures on their prior path; explicit facts, faults, and lease duration are opt-in at named rooted synthetic call sites.

The exact source assertions are documented in `acceptance-simulation.json`. Full scoped `VER-1` and `VER-2` on the execution tree each pass 1/1 with exit 0; raw outputs and direct exit files are under `verification/`. Both full-A tests fail at the intended baseline assertions before the fix. The exact B variant retains FIX-OX-02 and masks FIX-OX-06; B-isolated VER-1/2 pass in its own worktree-local target. The execution tree VER-1/2 also pass in the execution tree's own target. `cargo +nightly fmt --all --check` exits 0 without warnings. A supplemental stable rustfmt check also exits 0, with only the expected warnings that nightly-only repository formatting options are unsupported by stable.

## A/B and failure records

Patch version is `patch 2.0-12u11-Apple`. Reverse patches preserve canonical zero-context hunks and increment `new_start` by one only for `new_count=0`, as required by Apple patch behavior. The checkpoint A dry-run/apply removes FIX-OX-02 plus same-file later FIX-OX-06 hunks; B removes only FIX-OX-06 and retains FIX-OX-02. All recorded A/B dry-run and apply commands exit 0 without offsets, fuzz, rejects, or failed hunks. A's two target files hash exactly to the base revision.

A full-scope A copy initially could not compile while future FIX-OX-05/09 consumer hunks still referenced this card's scaffold. The isolated A baseline therefore also masks those three downstream consumer hunks (owner ordinals 45-47); after masking, all three affected test files hash exactly to base `ea0d3d5`. Both named tests run and reproduce `502/INTEGRITY_ERROR` instead of expected `200` or `413`; see `verification/A-full-baseline-VER-*.{stdout,stderr,exit}` and `dependency-consumer-mask-results.json`. A narrower A-red variant retaining the scaffold independently reproduces the same red results in `verification/A-red-post-VER-*.{stdout,stderr,exit}`. The first compile-only attempt remains preserved as a diagnostic, not counted as the behavior red.

An intermediate diagnostic used one `CARGO_TARGET_DIR` across multiple worktrees and exposed Cargo artifact reuse. Those runs are excluded from final acceptance. The authoritative B-isolated runs used B's own `target/`; the final execution-tree VER runs used the execution tree's own `target/`. Evidence logs replace local absolute paths with checkout labels to satisfy ER-11.

## Release boundary

No version bump, push, tag, Docker image, or release was performed. The only patch release remains OX-284 at the end of the plan.
