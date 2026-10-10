# FIX-OX-15 ER-05 review request — round 1

Perform an independent, read-only review using only `docs/plan/evidence/plan-20260920/cards/FIX-OX-15/review/round-1/snapshot/`. The snapshot has 30 payload files. Its manifest SHA-256 is `3852b4129b353dfd92968a485be7146bd8c6de124be88ab1d2b1c2dd484a9669`. Do not edit files or run tests. Treat the snapshot as the full review packet.

Review the test-only change against its frozen task card, the plan template and repository gates. In particular:

1. Verify source provenance: A must match base commit `1adbfef0e62eb99e9707253cbfe66ee49f3fe0cd`; B must match the submitted working source; the A/B diff should be limited to the planned test file and must not change production behavior.
2. Assess whether the publication-disabled fixture still exercises the intended actual HTTP chunk-map endpoint and PostgreSQL receipt path, and whether avoiding unrelated rooted-metadata route locks is a valid fixture choice. Use the retained advisory-lock diagnostic evidence; do not accept a weaker fake path.
3. Check AC-1 through AC-4 individually: PostgreSQL repository and budget installed before the held leader; receipt-write gate reached before callers; one same-source whole proof with each receipt rechecked; other storage's receipt fault fails closed. Verify that assertions and observed counters prove each criterion.
4. Recheck the exact A/B VER-1 command and raw exits, the fmt/clippy commands and exits, all recorded hashes, redactions, and the consistency of README/manifests/status. The repository-wide `cargo test --all` is currently running separately and has reported failures before completion; do not treat it as a pass. State whether this unfinished separate gate blocks the card-specific ER-05 review or only final submission.
5. Confirm no production code, version face, or release artifact was changed. The user has explicitly updated execution policy: a card commit may be pushed after review; bump/tag/release remain for OX-284. The user instruction governs execution where it conflicts with frozen no-push wording.

Report P0/P1/P2/P3 findings with packet paths and line numbers where practical, and distinguish blocking findings from non-blocking observations. End with exactly one literal verdict line: `VERDICT: PASS` or `VERDICT: FAIL`.
