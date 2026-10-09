# FIX-OX-08 本地验收证据

**状态：** `Lifecycle=in-progress`、`Acceptance=locally-accepted`（2026-10-09 20:21:58 UTC）。C/D 由 OX-284 承接，尚未执行。

**审查基线：** M0 计划 SHA `ff3c36e9c66522171053ec692385b8eea029c66da4e6142856e7d9bfdfbc1ddf`，Claude Code literal `VERDICT: PASS`；原始报告、234 文件只读快照、文件清单、排除清单和 review manifest 均在 [`review/`](review/)。review manifest SHA-256 为 `64c1d488b5b745c847296ad5d8d1fbdfade40b36a0751a27edc3ddb207085c68`。

## A/B 与验证

- 源差异来自 `ea0d3d5d2efe9d722c3f4defc8d7347503e2df47` → checkpoint `81f2e5ce1b26177f0b0956504b17129c8a5e2cec`。零 context owner 子 hunk 共 17 个；每个 `+/-` 行都有唯一 owner。规范 diff、owner manifest、canonical/application patch 分别留档。
- U0 源差异 SHA-256：`d4fa8ead487d101f6aec72ea0aa22c7b1b1761f5c122a66a18106d5e29bf8eda`。U3 源差异 SHA-256：`20f1a463fb40984d7b1d0ef495f8768653997224ce6ccce3cbc9b693b37b7082`；两者 changed-line projection 相同。
- A dry-run/apply 逆向 17 个 owner patch，Docker 文件精确恢复 base SHA `5f5af6405e95bbc961cce6aaeb231561c2ed933ceac9f02ff9138592e027472b`。
- B dry-run/apply 逆向 FIX-OX-11 / FIX-OX-01 的 10 个 owner patch，Docker 文件精确得到 FIX-OX-08-only SHA `6441158f544241f09e28e957a08eeff077382822500dabd5d0e5086dd35c99b3`。
- 两组 dry-run 均记录文件前后 SHA、patch 版本 `patch 2.0-12u11-Apple`、逐个退出码和原始 stdout/stderr。A/B 实际应用仅改变 `.github/workflows/docker.yml`；其他 6 个 source/test 文件与 checkpoint 字节一致。
- `VER-1`、`VER-2`（actionlint 1.7.12）、`VER-3` 均以最终精确命令退出 0。验收仅引用 `raw/VER/*-final.*`；较早一次 VER-3 harness 转义错误作为工具误调用保留并说明，不算验收结果。
- R26 patch 语义与 Libra 坐标 fixture 仍见 [`../../r26-method-preflight/`](../../r26-method-preflight/)。

## 审查与边界

Claude Code 检查了 FIX-OX-08 的 owner 切分、A/B、AC-1..AC-7、VER-1..VER-3，并返回 literal PASS；完整报告见 [`review/claude-review.md`](review/claude-review.md)。6 条 P3 的闭环见 [`R28-P3-closure.md`](R28-P3-closure.md)。M0 amendment/source-diff/raw-header 链证据引用于该闭环说明。

source/test/workflow WIP 已由原 M0 checkpoint 保存；此卡提交只登记本地验收与证据，不重复复制 WIP。本卡不 bump 版本、不 Push、不发布。下一张串行卡是 FIX-OX-11。
