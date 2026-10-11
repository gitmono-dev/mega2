# FIX-OX-17 evidence

State: the card's own VER-1 (three consecutive focused runs) reproduced the failure on the base source — **1 pass, 2 failures, both `Elapsed(())` at `snapshot_raw_blob_tests.rs:530`** — so the card took its "any failure → keep investigating, never close on an empty diff" branch. The fix is one test-only hunk that raises the barrier and the post-release window; three consecutive runs on the fixed source all pass, and `fmt` / `clippy` / `cargo build` / `cargo build --tests` are green.

## A/B

| Run | Source | Exit | Result |
|---|---|---|---|
| A-run1 | base (10 s / 10 s windows) | 0 | `1 passed; 0 failed; 2463 filtered out` (208.77 s) |
| A-run2 | base | **101** | panic at `src/api/router/snapshot_raw_blob_tests.rs:530:14`: `called Result::unwrap() on an Err value: Elapsed(())` (208.72 s) |
| A-run3 | base | **101** | same panic at `:530:14` (152.97 s) |
| VER-1 run1 | fixed (60 s / 30 s windows) | 0 | `1 passed; 0 failed; 2463 filtered out` (210.66 s) |
| VER-1 run2 | fixed | 0 | `1 passed; 0 failed; 2463 filtered out` (204.86 s) |
| VER-1 run3 | fixed | 0 | `1 passed; 0 failed; 2463 filtered out` (214.48 s) |

The failing line is the `.unwrap()` of the `timeout(Duration::from_secs(10), entered.notified())` barrier, i.e. exactly the mode recorded in the plan's historical failure map. `A-source.rs` matches the base blob (`700d966a…`), `B-final-source.rs` matches the worktree (`b3dd61af…`), and the diff is one test-only hunk changing two timing constants in `raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll`: the `entered` barrier 10 s → 60 s and the post-release window 10 s → 30 s. No assertion changed and no production source file is modified.

## Why the card did not close on an empty diff

The card states that closure with an empty code difference is allowed only "若三次重复 focused 均通过且 A/B 确认没有本卡 source hunk"; "若任一失败则继续调查，不得用空 diff 关闭失败". Two of the three base runs failed, so the empty-diff path is unavailable and the card must fix or escalate. The failure is not a production defect: the diagnostics below show the production revocation contract holding in all four cases, and the fault is the test's own 10 s budget.

Withdrawal and reinstatement, stated plainly: the barrier change was first drafted while only the round-1 isolated run (`A-VER-1`, 231.48 s, the run cited on the card) was available and that run passed, so the draft was withdrawn and the empty-diff path was attempted. Running the card's full three-run VER-1 gate then produced `A-run1` as a pass and `A-run2`/`A-run3` as failures, which removed the empty-diff path and reinstated the barrier change on that evidence. The ledger initially still described the withdrawn path; round 2 of review caught that, and the ledger now describes the delivered path.

## Historical records (cited exactly, not rewritten)

| Record | Reference | Content |
|---|---|---|
| Historical classification | `docs/plan/plan-20260920.md:275` | lists this witness as "held backend entered barrier 超时" in the historical 13-failure map |
| Checkpoint re-audit | `docs/plan/plan-20260920.md:292` (verified at pre-amendment SHA `24f1be3f…` and re-checked after this card's amendment) | "全量通过；focused exit=0 / 194.12s。历史 barrier 超时本轮未复现；该测试文件不在 checkpoint source diff 中" |
| Tree-wide failure list | `cards/FIX-OX-15/verification/full-suite-failure-attribution.md` line 48, SHA-256 `0d46f8da6c92e81397495a914a80fc4018241528ea9e2676bc8d472838c90bd7` | lists this witness among the 34 reported failures of that incomplete tree-wide run, whose own text says individual causes are not established |

The three records disagree by construction. This card's contribution is the first *reproduced* focused failure of the witness: two of three base runs fail at the same barrier line.

## Card amendment (ER-03 / ER-10)

The card's own text is amended in this change. `plan-20260920.md`'s FIX-OX-17 "Current evidence" now records the reproduced failure (base runs exit 0 / 101 / 101, `Elapsed(())` at `snapshot_raw_blob_tests.rs:530:14`), the production-semantics checks, the measured barrier margin, and the delivered test-only timing fix with its three passing VER-1 runs. The original empty-diff closure condition stays in the card as an untriggered alternative and as AC-5.

- Amendment evidence: `verification/source-restoration/plan-amendment.diff` (one replaced line, exit 1 from `diff -u`), recorded in `verification/source-restoration-check.json` as `plan_amendment`.
- Plan SHA before the amendment (the prior candidate value; R32 PASS is bound to `9d77fdd0…` and M0 to `ff3c36e9…`): `24f1be3fe8e431f664ced7eeb03d403a7793ce393d69aa162d663071566b444c`. Plan SHA after: `f1bb94367cfd8c878409d76ee53c9deca83f88f50de1b20d91119d7e689520f1`.
- `libra diff` elides this file as a large file (`<LargeFile>`), so the amendment diff is captured with `diff -u` against the committed blob instead of `libra diff`.
- The historical line citations (`plan-20260920.md:275` and `:292`) keep their line numbers across the amendment; they were verified at the pre-amendment SHA and re-checked at the current SHA.

## Diagnostics (hypothesis inputs, not proven production characteristics)

1. **Timing measurement** (`verification/diagnostic/diagnostic-run.stderr`, source = the final source's base plus one temporary `zz_diagnostic_fragmented_timing` test, removed afterwards): all four cases record `entered=true`, `status=200`, and a body that terminates with `LEASE_EXPIRED` after the lease is revoked, with `tail=0`, `drops=1`, `response_used=0`, `scratch_used=0`, `eof=true`, and the case-correct delivered prefix (0 bytes for cases 0/1, 1,048,576 bytes for cases 2/3). The barrier consumes `barrier_wait_ms` = 6,200 / 6,141 / 9,448 / 8,817 of a 10,000 ms budget, i.e. 62–94 % of it. The diagnostic measures wall-clock segments only and does not instrument `revalidate_access`, so attributing the cost to publication-enabled route verification inside the body path remains a **hypothesis**.
2. **Short-barrier probe** (`verification/diagnostic/short-barrier-probe.stderr`, source = a temporary copy with the 60 s/30 s windows plus one `zz_diagnostic_fragmented_barrier_margin` test): with a deliberately short 5 s barrier, cases 0 and 2 report `entered_within_5s=false` and `entered_within_60s=true`. The probe itself exits 0 and does not reproduce an exit-101 run; it is not claimed to.

## Acceptance criteria

- AC-1: asserted by the witness — `fixture.counts.assert(1, prefix_length)` immediately after the barrier; the diagnostics record `whole=1`.
- AC-2: the delivered prefix is asserted by the witness (0 bytes for cases 0/1; `CHUNK_SIZE` = 1,048,576 bytes for cases 2/3). The `LEASE_EXPIRED` termination is **diagnostic-supported**: the witness discards the stream error value, and no HTTP 410 is observable on this path because the response status is already 200 and the failure is a body-stream error.
- AC-3: asserted by the witness — `tail_polls == 0` and `drops == 1`.
- AC-4: asserted by the witness — `response_budget.used() == 0` and `scratch_budget.used() == 0`; the diagnostics add `eof=true`.
- AC-5: **not applicable as written** — the three base runs did not all pass, so the card takes the "any failure → keep investigating" branch and delivers the barrier fix instead of an empty diff. This is the section the card's own text uses for the failure case.

## Verification

- VER-1 (three consecutive runs, fixed source): exit 0 each; `1 passed; 0 failed; 2463 filtered out` at 210.66 s / 204.86 s / 214.48 s. Raw exit, stdout and stderr for each run are archived as `verification/VER-1-run{1,2,3}.*`.
- Baseline A (three consecutive runs, base source): archived as `verification/A-run{1,2,3}.*` with raw exits 0 / 101 / 101.
- `cargo +nightly fmt --all --check`: exit 0.
- `cargo clippy --all-targets --all-features -- -D warnings`: exit 0.
- `cargo build` and `cargo build --tests`: exit 0; both emit the pre-existing macOS linker `__eh_frame section too large` warning, which stays ledgered against OX-284 final C.
- `source .env.test && cargo test --all`: not run for this card; the repository-wide gate is owned by OX-284 final C.

## Follow-ups (non-blocking for this card)

- `FIX-OX-18`: this card measured a publication-enabled cost of roughly 9–10 s per request with up to 9.4 s inside the body-path barrier; the source contains a `revalidate_access` call before and after each backend poll, but that call is **not** instrumented by this card's diagnostics, so the extra poll cycle in cases 2/3 is a hypothesis. That measurement is recorded in the live ledger `docs/plan/plan-status.md` § "FIX-OX-17 当前卡审计记录" as an input FIX-OX-18 must check for whether it is environmental (for example, growing with the number of schemas in the shared test database) before it is called a production characteristic.

## Review record

- Round 1 (Claude Code, read-only): literal `VERDICT: FAIL`. P1-1 three consecutive VER-1 runs were not archived; P1-2 the historical failure was cited without an archived reference and contradicted the card text; P2-1 AC-2 was diagnostic-only with an unobservable 410 claim; P2-2 the mechanism was inferred; P2-3 the FIX-OX-18 handoff was not durable; P2-4 the executed path was not the card's; P3-1 redactions card label; P3-2 the worktree capture predated evidence finalisation; P3-3 margin-probe wording; P3-4 post-release window rationale; P3-5 linker warning ledger. Round 1 also accepted the code change, the A/B mechanics, the fix choice and the gates.
- Round-1 resolutions: P1-1 — the three required runs were executed; they reproduced the failure (2 of 3), which is why the earlier withdrawal was reversed and the barrier fix reinstated; both the failing baseline runs and the three passing VER-1 runs are archived (P1-2) and the historical records are cited with path, SHA and line; AC-2 is restated as witness-asserted prefix plus diagnostic-supported termination with the 410 claim removed (P2-1); the mechanism is labelled a hypothesis (P2-2); the FIX-OX-18 handoff is recorded in the live ledger (P2-3); the executed path is now the card's documented failure branch (P2-4); the redactions card label is corrected (P3-1); `worktree-status.stdout` is captured after the evidence set is final (P3-2); the probe wording is corrected (P3-3); the post-release window rationale is stated below (P3-4); the linker warning stays ledgered against OX-284 final C (P3-5).
- Post-release window rationale (P3-4): the diagnostics never timed release → finish, so 30 s is a margin choice rather than a measurement. It does not weaken detection: a source that keeps waiting on the held poll never completes, so `tail_polls == 0` and the case-correct prefix are still asserted, and a slower-than-30 s unwind would fail the run rather than pass it.
