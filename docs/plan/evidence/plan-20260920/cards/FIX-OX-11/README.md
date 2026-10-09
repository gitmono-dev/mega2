# FIX-OX-11 local acceptance

**Lifecycle / Acceptance:** `in-progress / locally-accepted` (2026-10-09 21:00:18 UTC). C/D is inherited from OX-284; this card does not bump, push, tag, release or publish Docker.

**Reviewed plan:** Claude Code final round 3 returned literal `VERDICT: PASS` for the pending card plan SHA `c28381c8b6da806e133c77245379767f63d899301bd7f1929815f6de9d1381d4`. P0/P1/P2=0; two non-blocking P3 notes are closed/accepted in [`P3-closure.md`](P3-closure.md). The 52-file review snapshot manifest SHA-256 is `4259ffd00d969f4b73aa64b48fa793c04a8bce545bde74f439eb6f1dfbb15df3`; final Claude report SHA-256 is `493f07cffbf95711c642da44882192fb18c568c9d7d365a3e95718fcc034cecc`.

## A/B evidence

- FIX-OX-08 is the accepted predecessor. Its source hunks are already part of the checkpoint coordinates; there are no source/test/workflow changes from checkpoint `81f2e5ce1b26177f0b0956504b17129c8a5e2cec` to review HEAD `346fe3c221b1f66636f8da6c151c0964b2a24800`, so the current-coordinate map is identity.
- A reverses 10 owner hunks from FIX-OX-11 and FIX-OX-01. Dry-run leaves checkpoint SHA `3a97771720765b30856b312667a2adc617282393457554d97ccfe883c7948712` unchanged; apply produces the independently replayed FIX-OX-08-only SHA `6441158f544241f09e28e957a08eeff077382822500dabd5d0e5086dd35c99b3`.
- B reverses the two FIX-OX-01 hunks. Dry-run leaves the checkpoint SHA unchanged; apply produces the independently replayed FIX-OX-08+11 SHA `c3b97f4709e1912b07b40f47a955e908908b217145e960e75232c1e1afab0eae`.
- Both dry-runs and both applies use `patch 2.0-12u11-Apple`; every patch exits 0, with no offset, fuzz, FAILED or reject diagnostics. The only changed path in A/B apply is `.github/workflows/docker.yml`; six other checkpoint source/test files match byte-for-byte.

## Verification

- VER-1: nine checks are all `True`, including exact matrix platform/runner/architecture/slug bindings, digest push/format, platform artifact names, missing-artifact failure, provenance, labels and `latest=false`.
- VER-2: actionlint 1.7.12 on the original B workflow exits 0 with no diagnostics.
- AC-1..AC-8 and VER-1..VER-2 are checked in the plan. Real tag-time two-architecture behavior remains OX-284 D.

The current plan SHA after acceptance status synchronization is recorded in `docs/plan/plan-status.md`; the pre-submit execution HEAD was `346fe3c221b1f66636f8da6c151c0964b2a24800`. The card acceptance and all noncredential evidence are committed once locally, without Push.
