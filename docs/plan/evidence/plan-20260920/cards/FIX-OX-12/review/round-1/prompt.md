# FIX-OX-12 ER-05 review request

Perform an independent, read-only card review using only the immutable packet at `../snapshot/`. Do not modify files or run tests. Treat the execution agent's narrative as claims to verify against the copied source and raw evidence. The plan candidate recorded in the copied status row is the reviewed context; this is a child card under G-12, so its card-level A/B and ER-05 evidence are local, while final-tree C/D are inherited from OX-284.

Review these questions:

1. Does B set a 3600-second lease before rooted resolve for both target tests, while matching the default fixture configuration apart from the lease?
2. Is the A/B source diff limited to those two fixture constructors? Are all original full-body, digest/header, source-read, alias receipt reuse, forged/missing receipt, stored-corruption, 502, and no-rebuild assertions retained?
3. Do both A runs fail with `LeaseExpired` before the targeted assertions, and do both B runs pass the exact commands recorded in `verification-results.json`? Verify exit codes, durations, source hashes, and output hashes against the packet.
4. Does `cargo +nightly fmt --all --check` pass? Are ER-11 path redactions explicit and reproducible from `redactions.json`? Does the packet avoid exposing local paths or credentials?
5. Does the card remain within its release boundary: no version bump, push, tag, Docker release, or claim of final C/D before OX-284?

Report any blocking or non-blocking findings with severity and evidence paths. End with exactly one standalone line: `VERDICT: PASS` or `VERDICT: FAIL`. A PASS requires the A/B scope, both focused runs, preserved assertions, evidence integrity, and release boundary to be supported by the snapshot.
