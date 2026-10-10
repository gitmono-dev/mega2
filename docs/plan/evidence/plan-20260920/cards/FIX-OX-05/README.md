# FIX-OX-05 evidence

State: A/B gates passed; Claude Code ER-05 round-1 returned literal `VERDICT: PASS`. The card remains `locally-accepted` until the plan release point supplies inherited C/D coverage.

## Scope and anchors

The card is in [plan-20260920.md](../../../../plan-20260920.md). The execution began from HEAD `b69827d7727c6e6d5124c05d311e195085bf781f`; the tested candidate source file SHA-256 is `ffd55289399ab38081b0384ace69754c6d9937603b74570bc7af08fb9cbd7955`. The A variant reverses only this card's fixture hunk and has SHA-256 `0063312eeae51a39ade6b9348b01af6067841632fd5318a17dc4ce64430e0ad6`. The sanitized A/B source diff is `verification/A-B-source.diff` (SHA-256 `b82f672a2d1375e38e62a0b11e6f0e41c0586c69f4e1a5250fdb2a2357eb22b0`).

## Acceptance evidence

- AC-1: B constructs a rooted fixture with `lease_seconds: Some(3600)` before rooted resolve. All 128 aliases and full body checks remain in the test.
- AC-2: the successful B run validates the first full 200 response, checks exact OID body loading once, then retains the second request's original 409 expected-digest assertion.
- A baseline: patch preflight and reverse application both exited 0 and changed only `snapshot_objects_bounded_tests.rs`. The test exited 101 after 654.71s: the second request returned 503 instead of the original 409.
- B initial attempt: exit 101 after 462.78s because the first request returned 503 instead of 200. The test helper did not retain the response body, so that attempt's cause is unclassified and it is not treated as acceptance evidence.
- B diagnostic rerun: a temporary, failure-only response-body message was added to the helper in the disposable B copy. The test passed in 771.23s. The temporary helper change was then reverted; the restored B source SHA returned to the candidate SHA above.
- B final VER-1: the uninstrumented candidate command exited 0: `1 passed; 0 failed; 2463 filtered out; finished in 775.63s`. It preserves all 128 alias inputs, full body verification, and the second-request 409 assertion.

The exact commands, exit files, sanitized stdout/stderr hashes, source hashes, and first failed B attempt are listed in `verification-results.json` and `ab-manifest.json`. `redactions.json` records ER-11 redaction of temporary execution paths; the raw path-bearing compiler output is not retained in the repository.

## ER-05 review

- Reviewer: Claude Code, read-only, round 1; exit code 0; literal verdict `VERDICT: PASS`.
- Report SHA-256: `e667133ca04f649e881b0c04c2299ab89262c7a15a5c512e18e71daddf71a512` (`review/round-1/claude.stdout`). Prompt SHA-256: `c69b01bb507b92d6143029e175019455c16a889b1c7209c7fbab7187137cbab1`. Snapshot manifest SHA-256: `2b4bc329015b20f94a3413c9b14ee3ca3b3098a6e0ee4b14a8ae40028cad0cdd`.
- The non-blocking P2 is tracked for OX-284's final-tree C gate: capture the response body and classify the first-request 503 seen in one of three B runs. FIX-OX-06's helper diagnostic does not cover the direct status assertion in `assert_objects`. The report also notes a non-blocking linker warning in B-final stderr.
- Review ran from 2026-10-10 09:52:51 UTC to 09:53:54 UTC. It accepted the card's A/B, VER-1, ER-11 redaction and release-boundary evidence; C/D remains inherited from OX-284.

## Release boundary

No version bump, push, tag, Docker image, or release is part of this card. OX-284 remains the sole patch release point.
