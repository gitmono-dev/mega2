# FIX-OX-13 ER-05 review, round 1

**Result: FAIL.** The code change and A/B evidence are correct, but the packet has no clippy result. The template requires a plan release child to run clippy itself (P1-1 below). That one gap is the only blocker; there are no P0 or P2 findings.

I reviewed only the snapshot, edited no repository files, and ran no tests.

## Integrity
- `snapshot-manifest.json` hashes to `8612ff0e…6514`. All 39 payload SHA-256 values match, and no files are missing or unlisted.
- The `code/` copies are byte-identical to the `evidence/verification/` copies.
- All A/B source hashes, stdout/stderr hashes, the diff hash (`2ab82242…`) and the FMT hashes match `ab-manifest.json` and `verification-results.json`.

## 1. Diff scope and meaning: OK
- Running `diff -u` again on the A/B copies reproduces `A-B-source.diff` exactly: 3 hunks, both diff exits 1, and only the three listed projections change.
  - `B-qualified_metadata_gc_tests.rs:89-93`
  - `B-qualified_metadata_source_revision_tests.rs:261`
  - `B-qualified_metadata_source_revision_tests.rs:279`
- Each projection now reads `CASE WHEN <bool> THEN 1::bigint ELSE 0::bigint END`. This is valid PostgreSQL and returns BIGINT.
- The `assert_eq!(…, 1)` checks are unchanged, so a true result still passes and a false result still fails. A NULL now becomes 0, which also fails the assertion.
- Only `*_tests.rs` files under the `…::tests::` module path change. No production SQL, schema or function return type is touched.
- The only other `::bigint` uses left in these files are `sum(incoming_refs)::bigint` (`B-qualified_metadata_gc_tests.rs:330,357`). That is a numeric-to-bigint cast and is valid.

## 2. A/B runs: OK
- **A runs:** `A-VER-1/2/3.exit` are all 101. Each `A-VER-n.stdout:10` shows `code: "42846"`, `cannot cast type boolean to bigint` from `transformTypeCast`. In VER-1 the error is at position 34, which is exactly where `::` sits in the original SQL.
- **B runs:** all three exit 0. Each `B-VER-n.stdout:5` reads `1 passed; 0 failed; 2463 filtered out`, and the full test paths match the card.
- **Format check:** `FMT.exit` is 0, and `FMT.stdout` and `FMT.stderr` are both empty (empty-file SHA `e3b0…`).

## 3. Consistency, linker warning, redaction: OK
- Commands match the card's VER-1..3 word for word, and the exit codes and harness times agree everywhere.
- The `__eh_frame section too large` linker warning is present in all six stderr files and is disclosed at `README.md:19`.
- The ER-11 redaction is recorded in `redactions.json`: one occurrence, the sanitized hash matches `B-VER-1.stderr` (with `[REDACTED_EXECUTION_PATH]` at line 1), and the original and sanitized hashes are both listed.
- Searching for `/Users`, `/Volumes`, `/private` and `/home` found no remaining absolute paths.

## 4. G-12 release boundary: OK
- `B-VER-1.stderr:1` shows `mega2 v0.42.25`, the card fields are `N/A`, and nothing in the packet records a bump, push, tag or Release. OX-284 remains the only release point (`README.md:29`).

## Findings

**P1-1 (blocking): clippy gate missing.**
- The rule: `context/plan-template.md:250` says the implementation-type row requires a `plan release child` to run "自跑 A 组 + fmt/clippy" (its own A group plus fmt and clippy).
- What the packet has: `verification-results.json:3` has only `format_check`; there is no clippy command, exit code or log anywhere. `README.md:3` claims the gates passed while listing only VER-1..3 and fmt.
- Inheriting OX-284's C gate does not cover this; the template requires the child to run clippy itself.
- **Fix:** run `cargo clippy --all-targets --all-features -- -D warnings` on the B tree. Record the exit code, stdout and stderr with ER-11 redaction. Add it to `verification-results.json` and `README.md`, then submit a round-2 snapshot. No source change is needed.

**P3-1: the A-run binary build is not in the packet.** The A stderr files show `Finished … in 0.65s` with no `Compiling` line (`A-VER-1.stderr:6`). Also, base commit `7c15d36f…` cannot be matched to the A copies from inside the packet. The error positions are consistent with the A SQL, so this is a residual note only.

**P3-2: the linker warning conflicts with the 0-warning rule.** It already appears in the A runs, so this card did not introduce it. Still, AGENTS.md requires `cargo build --tests` to show 0 warnings, so OX-284's final C gate on macOS must deal with it explicitly.

**P3-3: wording of "locally accepted".** `README.md:3,25` says the card becomes locally accepted only after a review PASS. Template line 197 orders it the other way: A/B pass → `locally-accepted` → ER-05. This matches how earlier cards were handled in the status row, so it is a wording fix only.

No residual-risk acceptance is named for this round; P1 cannot be accepted that way under ER-05.

VERDICT: FAIL
