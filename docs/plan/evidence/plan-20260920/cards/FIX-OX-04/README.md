# FIX-OX-04 evidence

State: A/B gates passed; Claude Code ER-05 returned literal `VERDICT: PASS`; the three non-blocking P2 recordkeeping findings are accepted as residuals by the Codex execution agent. This audit card changes documentation/evidence only; no source, test, configuration, or workflow changes are part of it.

## Scope and anchors

The card and its acceptance criteria are in [plan-20260920.md](../../../../plan-20260920.md). Current frozen plan SHA-256: `9d77fdd00b291fabbf960c41cca35025e5b81d3c9fbb35d75d63d3e3a4bdf75e`. Relevant anchors: `docs/plan/plan-20260920.md:263` (card), `docs/plan/plan-20260920.md:285` (full-run evidence), `docs/plan/plan-20260920.md:304` (per-failure comparison), and `docs/plan/plan-20260920.md:304` (acceptance criteria).

## Test result

FIX-OX-04 VER-1 is a diagnostic run, so exit 101 is expected when it reports failures. The checkpoint-inclusive run completed with 2448 passed, 13 failed, and 3 ignored in 18457.34 seconds at HEAD `c10776d9e84d2301e953d673fe0e0c70dad1fb1a`. The original log remains in `/tmp` and is not copied into the repository; its SHA-256 is `9670b9443028a0cce77ef2b2e00fd6a643067edbab488d6735f4e27381b2c3ba`. VER-2 was re-run read-only: exit 0, 136 matching lines, output SHA-256 `e3c665ce644cc1a86f53dce0b3025dfc892ca6ba1aab1b895de0e85ebdb91042`.

All 13 failures are listed in `review/snapshot/verification/failure-summary.txt` and cross-referenced to the focused results and named owner cards in the plan. The plan distinguishes the historical 13, checkpoint failures, checkpoint-altered outcomes, and newly observed failures. A source/test/workflow diff from the tested HEAD to current M0 commit `4ee151795cd44c2301f20ea1135c1e128083b1de` is empty, so the existing diagnostic remains applicable without another full-suite run.

## Release boundary

This is a `no-release` audit card under REL-OX-01. No version bump, push, tag, Docker image, or release is part of this card. OX-284 remains the sole patch release point.

## Review and accepted residuals

Claude Code ER-05 report: SHA-256 `49397676048417bd61210e4a5d3d5614fc242156c5446022687f8f3389ff7320`; exit 0; literal `VERDICT: PASS`; no P0/P1 findings. The reviewer listed three P2 documentation/evidence gaps. The named execution owner accepts them as residuals: no contemporaneous OS process census was retained, the Acceptance label is joined to the prior paragraph, and exact focused shell invocations were not recorded. The acceptance is recorded in `review/round-1/p2-acceptance.md`; none changes the 13 failure assignments or task boundary.
