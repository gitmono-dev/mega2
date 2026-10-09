You are the independent Claude Code ER-05 reviewer for Mega2 plan card FIX-OX-09. This is a read-only review. Do not edit files or run tools that write.

Review only this immutable packet: ${FIX_OX_09_REVIEW_SNAPSHOT}
Snapshot manifest SHA-256: 14546d4d8e36841eb6ea37cab9fc3dfc1a0ac9d6e7af0a7285130bd4549839df
The packet's `snapshot-manifest.json` lists every payload file; verify every listed size and SHA-256 and confirm the copied candidate source matches `evidence/verification-results.json`. The frozen plan SHA is 46d83eb6cf655ddbb96f80f960878c825691954e91b8c6f1515032768dbd9d74. Candidate parent HEAD is 711b6d6b7d671f80842c0fa861d5f2b26c30ef82.

Audit the card requirements in `context/task-card.md` and repository constraints in `context/AGENTS.md`. Inspect the actual code diff, A and B outputs plus direct exit files, and the diagnostic exclusions. Determine whether each AC-1..AC-6 and VER-1 is genuinely evidenced, whether the write set is limited to `src/api/router/snapshot_objects_bounded_tests.rs`, and whether any production or shared fixture code changed. The repository's release rules defer version bump/push/tag/release to OX-284.

Pay particular attention to AC-2: the final pre-body 502 case seeds two different OIDs with one shared digest and conflicting sizes (8192 vs 8193); the second seeded digest also differs from its raw body digest. The separate size-only diagnostic returned 502 after reading both bodies and is explicitly excluded. Decide strictly whether the final named shared-digest conflicting-size case satisfies the card as written; do not describe it as a size-only proof. AC-3 is independently exercised by a correct-fact 200 path that validates both distinct OIDs. AC-4 is independently exercised with a wrong expected digest after correct facts.

Do not treat README statements as proof; check source and captured outputs. Return concise findings and a final standalone verdict line exactly `VERDICT: PASS` or `VERDICT: FAIL`. Use FAIL if any P0/P1 remains or if the evidence does not support the card's stated acceptance. Do not infer or add release completion.
