# FIX-OX-17 ER-05 review, round 1

## Summary

The code change is small, test-only and safe. It is one hunk at `code/A-B-source.diff` @@ -525: the `entered` barrier goes from 10 s to 60 s and the post-release window from 10 s to 30 s. No assertion changed and no production file changed.

The diagnostics are useful and mostly support a barrier-margin explanation. The card cannot pass yet:
- the card's own VER-1 was not executed as written, and
- the failure the fix is said to address is not archived in the packet. The card's own text contradicts it.

## Findings

### P1 (blocking)

**P1-1: VER-1 was not run as specified.**
- `context/task-card.md` VER-1 requires running the focused command **three consecutive times**, each with raw exit 0 and `1 passed; 0 failed`.
- The packet has one A run (`evidence/A-VER-1.*`) and one B run (`evidence/B-final-VER-1.*`). Neither `verification-results.json` `runs[]` nor the README records three B runs.
- The requirement is unconditional, not limited to the AC-5 branch.
- This matters more for a timing-robustness fix, where repeated passes are the evidence that matters.
- Fix: archive three consecutive B-final runs, each with its own exit, stdout and stderr files.

**P1-2: The failure being fixed is not in the archive, and the card text contradicts it.**
- The README ("State" and "Root cause" ¶1) and `ab-manifest.json` `a_source.variant_note` cite a "recorded FIX-OX-04 baseline … focused exit 101 … `Elapsed(())`". They also say it "appears again in the FIX-OX-15 tree-wide failure list".
- No artifact, path, SHA or line reference for that record is in the packet.
- `context/task-card.md` "Current evidence" says the opposite: the witness passed in both the checkpoint full run and the focused run (exit 0, 194.12 s).
- The whole justification for a source delta, instead of the card's AC-5 evidence-only path, rests on this record.
- This is the same defect class as FIX-OX-16 round-1 P1-1.
- Fix: cite the exact archived artifact with path and SHA, for example the FIX-OX-15 `full-suite-failure-attribution.md` entry and its run conditions. Then reconcile the card's Current evidence with it.

### P2 (non-blocking individually; should be fixed with the P1s)

**P2-1: AC-2 "terminates with `LEASE_EXPIRED`" is not asserted by the witness.**
- The witness drops the error: `let (delivered, _, mut stream) = …` in `code/B-snapshot_raw_blob_tests.rs` (raw_post_await test, after `finished`).
- Only the diagnostic stderr shows `LEASE_EXPIRED`.
- The README AC-2 line also claims "(`410`, non-retryable)". Nothing observes a 410 here: the response status was already 200, and the termination is a body-stream error.
- Fix: either assert the error code in the witness, which is an assertion addition the card's AC permits, or restate AC-2 as diagnostic-supported and drop the 410 claim.

**P2-2: The mechanism is inferred, not measured.**
- The README says "publication-enabled route verification inside the body path" and "an extra `revalidate_access` cycle … because `require_eof` polls again" for cases 2 and 3.
- `diagnostic-source.rs` only measures wall-clock segments. Nothing instruments `revalidate_access`, and no production source is in the packet to check against.
- "Passes in isolation but fails under load" is also not demonstrated by any loaded run.
- Fix: label these as hypotheses, or add instrumentation.

**P2-3: The handoff to FIX-OX-18 is not durable, and the existing ledger contradicts it.**
- The README says the 6–9.4 s body-path cost is "owned by the pre-existing card FIX-OX-18".
- The FIX-OX-16 ledger in `context/plan-status.md` §四 explicitly excludes this witness from the FIX-OX-18 handoff.
- `plan-file-diff.stdout` is empty, so no ledger or plan amendment records the new measurement or the transfer of ownership.
- This repeats FIX-OX-16 round-1 P2-1.
- Fix: record it in the FIX-OX-18 handoff in the plan or status ledger.

**P2-4: The card text was not reconciled with the path taken (ER-03).**
- The card's AC-5 and Current evidence describe only two outcomes: three passes with no source delta lead to evidence-only closure, and a failure leads to further investigation.
- The executed path, a hardening source delta while the test passes, is not reflected in `docs/plan/plan-20260920.md` (plan diff is 0 bytes).

### P3

- **P3-1: Wrong card label.** `evidence/redactions.json` has `"card": "FIX-OX-16"`.
- **P3-2: `worktree-status.stdout` predates evidence finalization.** It lacks README.md, ab-manifest.json, verification-results.json, source-restoration-check.json, tracked-source-diff*, plan-file-diff* and its own `.exit`. This is the same issue as FIX-OX-16 round-1.
- **P3-3: The margin probe does not "reproduce exactly" the recorded failure.**
  - The README claims the probe "produces exactly the recorded `Elapsed(())` exit 101".
  - The probe used a 5 s barrier, not 10 s, logged `false`, and exited 0.
  - What it actually shows is that a barrier shorter than the measured wait yields `Elapsed`.
- **P3-4: The 30 s post-release window has no measurement behind it.**
  - The diagnostics never timed release→finish.
  - The change is harmless: a continued held poll hangs indefinitely, so detection is unaffected, and `tail_polls == 0` is still asserted.
  - Still, state the rationale.
- **P3-5: The linker warning is not separately ledgered.** The `__eh_frame` warning in `cargo build` and `cargo build --tests` is consistently deferred to OX-284. It is noted here only so it stays tracked in the ledger.

## Answers to the review questions

**1. Root cause and honesty.**
- **Plausible and partly supported.**
  - `diagnostic-run.stderr` shows all four cases reach the held poll.
  - Barrier waits are 6,200, 6,141, 9,448 and 8,817 ms against a 10 s budget, about 5.5–38 % headroom.
  - The 5 s / 60 s probe shows a short barrier expires before the held poll, while a longer one reaches it.
- **Not a wrong-assertion fix:** no assertion changed.
- **Production semantics are correct:** revocation yields `LEASE_EXPIRED`, `tail=0`, `drops=1`, both budgets at 0, and `eof`.
- **Not sufficient yet:**
  - The original exit-101 record is unarchived and contradicted by the card text (P1-2).
  - The "route verification / `revalidate_access`" mechanism is inferred (P2-2).
- **Isolation result is recorded honestly:** the README and `A-VER-1` (exit 0, 231.48 s) both say plainly that the base passes in isolation.

**2. A/B and scope.**
- `A-source` matches the base blob: 700d966a…, `matches_a_source: true`.
- `B-final-source` matches the worktree: b3dd61af….
- The diff is one hunk changing two constants, and `diff` exits 1 as expected.
- `tracked-source-diff-stat` shows only `snapshot_raw_blob_tests.rs` changed (2 lines in, 2 out). No production source changed.
- I confirmed the inline A and B sources differ only at those two lines.

**3. Acceptance criteria.**
- **AC-1** (`counts.assert(1, prefix_length)` after the barrier): asserted by the witness and supported by `whole=1`, `bytes=1` / `1048689`.
- **AC-3** (`tail_polls == 0`, `drops == 1`): asserted.
- **AC-4** (both budgets 0, plus `eof`): asserted.
- **AC-2:**
  - The case-correct prefix is asserted: 0 bytes for cases 0/1, `CHUNK_SIZE` (1,048,576) for cases 2/3.
  - The `LEASE_EXPIRED` termination is diagnostic-only (P2-1).
- **AC-5:** not applicable as written, because there is a source delta (P2-4).

**4. Fix choice.** Defensible.
- Keeping the publication-enabled fixture preserves coverage of the publication-enabled body path while the witness passes.
- FIX-OX-16 switched fixtures because its baseline failed deterministically; that is not the case here.
- The FIX-OX-16 ledger already assigns this witness to FIX-OX-17's own gate, so the reasoning is consistent.
- The new latency observation still needs a durable home (P2-3).

**5. Gates.**

| Gate | Exit | Notes |
|---|---|---|
| `cargo +nightly fmt --all --check` | 0 | empty output |
| `cargo clippy --all-targets --all-features -- -D warnings` | 0 | |
| `cargo build` | 0 | pre-existing linker warning |
| `cargo build --tests` | 0 | pre-existing linker warning |
| `source .env.test && cargo test --all` | not run | stated honestly as owned by OX-284 final C |

Deferring `cargo test --all` matches G-12 plan-child inheritance and the FIX-OX-15/16 precedent.

**6. No version bump, tag, Release or push.**
- Compiles still report `mega2 v0.42.25`.
- `worktree-status` shows only the test file modified, plus untracked evidence. There is no `Cargo.toml` or `Cargo.lock` change.
- The branch line is `## main...origin/main` with nothing ahead.
- No tag, Release or push artifacts exist.

## Required for round 2

1. Three consecutive B-final VER-1 runs, archived.
2. The exit-101 baseline, archived with path and SHA, and the card's Current evidence reconciled with it.
3. Fixes for P2-1 through P2-4: assert or restate AC-2, mark the mechanism as hypothesis, amend the FIX-OX-18 handoff in the ledger, and revise the card text.
4. Fixes for P3-1 and P3-2: correct the `redactions.json` card label and re-capture `worktree-status` after evidence is final.

VERDICT: FAIL
