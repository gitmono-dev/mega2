# FIX-OX-09 evidence

State: candidate; A/B, VER-1, and nightly format evidence recorded. Claude Code ER-05 review is pending.

## Scope

This card changes only `src/api/router/snapshot_objects_bounded_tests.rs`. It consumes the accepted FIX-OX-02 rooted fixture scaffold without modifying shared fixture construction or production fail-closed logic. The frozen plan SHA-256 is `46d83eb6cf655ddbb96f80f960878c825691954e91b8c6f1515032768dbd9d74`; the parent execution HEAD is `711b6d6b7d671f80842c0fa861d5f2b26c30ef82`.

## Acceptance evidence

- AC-1: `Fixture::new_rooted_with_options` seeds the facts before resolving a rooted publication lease.
- AC-2: the pre-body rejection case presents two different OIDs with the same seeded digest and conflicting sizes (8192 vs 8193); the second seeded digest also conflicts with its raw object. The candidate returns `502/INTEGRITY_ERROR` with zero body reads. A separate size-only trial returned 502 after two body reads and is explicitly excluded; this card's evidence does not claim a size-only proof.
- AC-3: with correct facts and request digests, `assert_objects` validates a successful 200 response containing both distinct OIDs; counters show two bodies and 16384 bytes.
- AC-4: with restored correct facts and a deliberately wrong expected digest, the route returns `409/EXPECTED_DIGEST_MISMATCH` before body reads.
- AC-5/AC-6: only the named test file changes; production behavior and the FIX-OX-02 shared constructor remain untouched.
- VER-1: the exact planned focused Cargo test exits 0 (1 passed, 2463 filtered). `cargo +nightly fmt --all --check` also exits 0.

A-consumer-masked exits 101 at the intended assertion (409 instead of the required pre-body 502), establishing the fixture-seed dependency. Intermediate failed attempts are retained and explicitly excluded in `verification-results.json`. Compiler stderr execution-tree paths were redacted under ER-11; transformations are bound by `redactions.json`.

## Release boundary

No version bump, push, tag, image, or release. This remains a child of REL-OX-01; final C/D stays at OX-284.
