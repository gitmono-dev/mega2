# User execution instructions in force (quoted from the plan ledger)

Source: `docs/plan/plan-status.md` and the FIX-OX-15 card README, both committed in the execution copy before this review.

1. 2026-10-09 (plan `plan-20260920.md`, G-12): the whole plan bumps and releases exactly one patch at the end; before OX-284 the members must not change the package version, push the plan branch/tag, create a GitHub Release, or trigger a versioned Docker release.
2. 2026-10-10 (user update): after each task card passes its review and is committed, that card commit may be pushed. Version bump, tag, and GitHub Release remain reserved for the terminal card OX-284.
3. The later instruction governs execution despite the frozen no-push wording in the plan, and it does not relax any A/B, ER-05, or evidence requirement per card.
4. Live GitHub acceptance (OX-24/OX-06) and the unique final versioned Docker release (OX-284 D-OX-TAG) are unaffected by this card.
