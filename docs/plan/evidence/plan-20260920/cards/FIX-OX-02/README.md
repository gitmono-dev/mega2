# FIX-OX-02 evidence

State: locally accepted after Claude Code ER-05 round-2 literal `VERDICT: PASS`; A/B variants and focused checks are recorded with the user-authorized local commit of all current noncredential changes. No push, version bump, tag, or release.

## Scope and source history

FIX-OX-02 owns the two bounded-chunk rooted fixtures and the shared `FixtureOptions` / `InitialFact` scaffold. FIX-OX-05/09/12 are consumers; FIX-OX-06 owns separate diagnostics hunks in the same source file. The accepted predecessors are FIX-OX-08, FIX-OX-11, and FIX-OX-01. The frozen plan SHA-256 remains `46d83eb6cf655ddbb96f80f960878c825691954e91b8c6f1515032768dbd9d74`.

Checkpoint raw hunk numbers and owner ordinals are separate fields in `owner-coordinate-map.json`; patch filenames label checkpoint raw hunk numbers explicitly. The current parent-to-candidate delta is `post-checkpoint-source.diff` with six zero-context hunks mapped to owner ordinals 49-54. B-r2 starts at parent `30ade799cba59ed45b67504cbe68f20b68b81f92`, applies that full candidate delta, then reverses only FIX-OX-06 raw checkpoint hunks 35-38. Its receipt and source hashes are in `B-r2-variant-results.json`.

## Acceptance evidence

- AC-1/AC-2: both large synthetic rooted fixtures seed size and SHA-256 matching the simulated stream before native publication and rooted resolution.
- AC-3: certified DB facts are not modified after publication; later expected-digest assignments update only the in-memory request expectation.
- AC-4/AC-5: batch budget and later-invalid-path rejection remain `413/LIMIT_EXCEEDED` and `404/PATH_NOT_FOUND`, with zero body reads before either rejection.
- AC-6: default `FixtureOptions` leaves `lease_seconds=None`, and the shared resolve request omits `lease_seconds` unless a named fixture explicitly opts in. Default constructors continue to use `FixtureOptions::default()`.

B-r2 VER-1/VER-2 and execution-tree VER-1/VER-2 after the AC-6 correction pass with exit 0. Raw outputs and direct exit files are under `verification/`. `cargo +nightly fmt --all --check` after the correction exits 0. A-full baseline VER-1/VER-2 fail at the intended pre-fix behavior assertions (`502/INTEGRITY_ERROR` instead of `200` or `413`); the downstream consumer masks and exact base hashes are separately recorded.

## A/B and failure records

The full A baseline reverses FIX-OX-02 and FIX-OX-06 checkpoint hunks and masks owner ordinals 45-47 (FIX-OX-05/09 consumers) so all affected files return exactly to base `ea0d3d5`. The earlier B-isolated attempt is excluded because it lacked receipts binding the post-checkpoint correction. B-r2 supersedes it with a detached parent worktree, complete six-hunk correction, exact FIX-OX-06 masks, local target, per-patch SHA values, and direct test exits.

The earlier A-red logs, shared-target B logs, and compile-only A attempt remain as explicitly excluded diagnostics in `verification-results.json`; they do not count toward acceptance. The linker stderr may contain the repository's Darwin compact-unwind warning; compiler/worktree absolute paths in evidence logs are redacted for ER-11.

P2 disposition: the synthetic repository intentionally has `/file`, `/alias`, `/executable`, `/nested/file`, and `outside` reference the same content-addressed blob. The verified fact is therefore object-scoped across those paths, not a path-local fact. The FIX-OX-02 test owner accepts this as the intended fixture semantics; no production code was changed, and the tests assert the blob identity and body/fact contract.

## Release boundary

No version bump, push, tag, Docker image, or release was performed. FIX-OX-02 is a plan-release child; C/D remain inherited from OX-284, and the only patch release remains at the end of the plan. After Claude literal PASS, the user authorized one local commit of all noncredential uncommitted files, including code, tests, workflow WIP, and this evidence; no push.
