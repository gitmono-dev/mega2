# FIX-OX-04 P2 residual acceptance

Accepted by: Codex execution agent (task executor)
Accepted at UTC: 2026-10-10T08:26:00+00:00
Claude Code ER-05: `VERDICT: PASS`, report SHA-256 `49397676048417bd61210e4a5d3d5614fc242156c5446022687f8f3389ff7320`, exit 0; no P0/P1 findings.

The following P2 recordkeeping findings are accepted as non-blocking residuals for this audit card:

1. The full run used one serialized Cargo test process with `--test-threads=1`; no second test stack was launched by this plan execution. A contemporaneous OS-level process census was not retained, so unrelated external processes cannot be ruled out.
2. `**Acceptance criteria:**` shares a line with the preceding conclusion paragraph. This formatting issue does not change the criteria or evidence. The R32-reviewed plan SHA remains unchanged.
3. Per-test outcomes and durations are recorded, but the exact focused shell invocations were not preserved. No command is reconstructed and claimed as historical fact.

These residuals do not change the 13 full-run failures, their focused outcomes, unique owner cards, conditional FIX-OX-23 → FIX-OX-19 branch, or the no-release boundary. The card remains `locally-accepted` pending OX-284's inherited C/D coverage.
