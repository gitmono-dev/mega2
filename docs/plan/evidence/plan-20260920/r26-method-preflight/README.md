# R26 method preflight evidence

This directory preserves the preflight supporting the R26 fail-closed amendment. It does not accept FIX-OX-08 or any other task card.

- `baseline-plan-8109d374.md` is the original R24-reviewed plan bytes from the signed M0 checkpoint; SHA-256: `8109d374ee3420414542bd1d7a09f4302050a5000027c8e242186b76020089c6`.
- `apple-patch-semantics-preflight.py` and `.json` record 13 exact-byte Apple `patch 2.0-12u11-Apple` fixtures, including ordered adjacent-owner reverse checks and an untransformed negative control. Result: PASS.
- `libra-zero-count-coordinate-fixture.py` and `.json` record four canonical zero-count coordinates emitted by temporary Libra repositories. Result: PASS.
- The Apple fixture's forward byte reconstruction uses the canonical full-source diff and its no-newline marker; Apple reverse-application patches are separately transformed only for `new_count=0`.

The exact plan under review is recorded in the separate review manifest. A/B over the real FIX-OX-08 workflow must still be rerun against that frozen plan after Claude Code returns literal PASS.
