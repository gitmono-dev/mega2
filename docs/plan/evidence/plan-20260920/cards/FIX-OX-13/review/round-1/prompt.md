# FIX-OX-13 ER-05 review request — round 1

Perform an independent, read-only review using only `docs/plan/evidence/plan-20260920/cards/FIX-OX-13/review/round-1/snapshot/`.
The snapshot contains 39 payload files. Its manifest SHA-256 is `8612ff0ee5b7d4be7bed7c11913fa1104814dcd23f84da71209fe737b81b6514`; the local integrity check records `issues: []`.
Do not edit repository files or run tests. Treat the snapshot as the complete review packet.

Review the source change and its A/B evidence against the task card and applicable repository/template rules. In particular verify:

1. The diff changes exactly the three test-only PostgreSQL boolean projections identified by FIX-OX-13, uses a valid CASE mapping with BIGINT results, preserves true/false assertion semantics, and does not change production SQL or unrelated behavior.
2. Each A run reproduces SQLSTATE 42846 on the original source; each B run passes the corresponding focused test, and the formatting check is captured with its exit and raw output.
3. A/B source hashes, diff scope/hunk count, commands, exit codes, test results, and sanitized log hashes agree across the packet. The linker warning is disclosed, and ER-11 path redaction is fully recorded.
4. The evidence respects the G-12 release boundary: no patch bump, push, tag, or release before OX-284. Identify any P0/P1/P2/P3 findings and cite packet paths and lines where practical.

Return a literal final verdict line: `VERDICT: PASS` or `VERDICT: FAIL`, following ER-05. Clearly label all P2/P3 findings and any named residual-risk acceptance.
