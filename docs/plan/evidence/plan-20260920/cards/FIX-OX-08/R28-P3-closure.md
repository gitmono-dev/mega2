# FIX-OX-08 Claude ER-05 P3 闭环

本说明补充 Claude 对 FIX-OX-08 的原始审查报告与原快照，不修改 owner source 变更或 A/B 接受结果。原报告仍保存在 [`review/claude-review.md`](review/claude-review.md)，原 234 文件快照由 `review/snapshot-files.sha256` 校验。

1. **P3-1，早期 VER-3 harness 误调用。** 较早的手工 shell harness 对 `run: |` 进行了过度转义并退出 1；它不是计划中的验收命令。该次原始输出仍在 `raw/VER/VER-3.*`。最终验收只采用 `raw/VER/VER-1-final.*`、`VER-2-final.*`、`VER-3-final.*`，三项均退出 0；精确计划命令的 VER-3 结果也在 `VER-3-exact.*`。README 与卡验收记录只引用最终成功结果，并把早期失败标为 tooling misfire。
2. **P3-2，统一 diff context。** 源 U0 diff SHA 是 `d4fa8ead487d101f6aec72ea0aa22c7b1b1761f5c122a66a18106d5e29bf8eda`；按计划要求生成的 U3 diff SHA 是 `20f1a463fb40984d7b1d0ef495f8768653997224ce6ccce3cbc9b693b37b7082`。`owner-hunk-manifest.json` 记录两种 diff 的 changed-line projection SHA 一致，说明 owner 拆分结果相同；完整 U0/U3 文件均留档。
3. **P3-3，前驱映射。** `owner-hunk-manifest.json` 明确记录 `accepted_predecessor_mappings: []` 与 identity coordinate mapping。FIX-OX-08 是串行链首卡，应用时没有已接受前驱造成的位移。
4. **P3-4，dry-run 不变性和工具版本。** `A-dry-run-final.json` 与 `B-dry-run-final.json` 均记录 `patch 2.0-12u11-Apple`，且 `workflow_sha256_before == workflow_sha256_after`、`dry_run_unchanged_file=true`；A/B apply 结果也记录每个 patch 的 SHA、退出码和预期/实际 Docker 文件 SHA。
5. **P3-5，M0 amendment 证据和计划行 86。** 本包现在链接 [`../../m0-source-diff-post-amendment.json`](../../m0-source-diff-post-amendment.json)、[`../../m0-raw-header-recheck.json`](../../m0-raw-header-recheck.json)、[`../../verify-raw-commit-headers.py`](../../verify-raw-commit-headers.py)，并记录执行头 `0d10dad7c80f689d7543e5b64b6331b5c3cb69b8`。这些记录把原 checkpoint、R27 amendment `9a6d705364cad0f501e3f081e524b7225059001d`、R28 amendment `c55d9ce24122c239b1e4f2e3235f6fb35120cf8f` 串成同一提交链；helper 复核 Signed-off-by 邮箱匹配和 gpgsig 头（不做密码学验签）。计划行 86 所述的 R27 先决条件仍为真，R28 amendment 是其后继，并未替换 checkpoint。此次 A/B 的 `--unified=3` diff、全量源 hunk、checkpoint 与 WIP source SHA 证据均已归档。该 P3 以为卡验收证据补充链路引用关闭，无需改动已审计划结构。
6. **P3-6，中间 B 状态的 mutable tag 写法。** B 是只在临时本地副本中的 owner-only 状态，未 Push、未运行发布工作流。FIX-OX-11 和 FIX-OX-01 分别拥有后续 digest/artifact 与 manifest 行；完整 checkpoint 保留在本地。OX-284 仍是整份计划唯一的 patch bump / Docker 发布点，因此该条为信息性边界，不影响 FIX-OX-08 本地验收。

## 暂存前空白检查说明

全量 `libra diff --staged --check` 仅报告 5 处保留空白：精确快照 `review/snapshot/plan-card-context.md:17`，以及 byte-exact U3 源 diff `source-unified3.diff` 的第 56、60、74、80 行。这些字节属于捕获的计划行/源 diff 上下文，删改会改变已审快照或原始 diff；其余路径没有该诊断。计划、README、状态表、Claude 报告、manifest 与本闭环说明的路径限定 diff 检查均通过。

Claude 原始结论为 literal `VERDICT: PASS`，无 P0/P1/P2。以上闭环说明是证据补充，审查快照仍精确保留原来的审查输入。
