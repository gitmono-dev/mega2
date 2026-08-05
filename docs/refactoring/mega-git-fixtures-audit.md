# Mega `tests/` Git fixtures audit（plan-20260803 / GM-09）

## Revision

- **Pin SHA:** `42cd288d266dade371aed03917d222eb57f1d5a4`
- **Check date:** 2026-08-03
- **Verified by:** `cd ../mega && libra rev-parse HEAD` → exact equality with DEP-04

## Tree and size

| Path | Approx size | Notes |
|---|---|---|
| `tests/` total | 9.3 MiB | 25 files |
| `tests/data/` | 9.3 MiB | packs + index + sparse loose objects |
| `tests/diff/` | 12 KiB | two small diff blobs |
| `tests/objects/` | 4 KiB | only `.gitkeep`（空） |
| `tests/refs/` | 12 KiB | `heads/master` → `5bb8ee25bac1014c15abc49c56d1ee0aab1050cb` |
| `tests/scripts/` | 8 KiB | `push.sh` 推送编排，非夹具再生器 |

Largest files (all under 5 MiB individually, but opaque pack binaries):

- `tests/data/packs/ref-base.pack` ≈ 4.43 MiB
- `tests/data/packs/pack-f8bbb573….pack` ≈ 4.21 MiB

Loose objects under `tests/data/objects/` do **not** contain tip `5bb8ee25…`; the advertised ref cannot be resolved without pack material.

## License and provenance

- Mega root dual-license: `LICENSE-MIT` + `LICENSE-APACHE`（Web3 Infrastructure Foundation）。
- `tests/README.md` 为空（0 bytes）；无夹具专用 SPDX/来源说明、无第三方数据声明。
- 可将根许可视为可再分发前提，但缺少「这些 pack/对象如何生成、是否含第三方内容」的夹具级 provenance。

## Generation

- `tests/scripts/push.sh` 只对既有 git 工作树批量 `git push` 到 Mega HTTP，**不能**从源再生 `tests/data/packs/*` 或 refs/objects。
- 仓内未发现 pack 生成脚本、seed 说明或 `Regenerate:` 文档。
- 因此最小移植后无法在 monoengine 内复现/更新夹具；一旦 tip 漂移只能整包再拷。

## Decision

**Decision: NO-GO**

理由（充分）：

1. `refs/heads/master` tip 无法仅靠 loose objects 解析，最小 refs+object 守卫必然拖入多兆 pack。
2. pack 无再生路径与夹具级 provenance；整树/pack 拷贝违反本计划「不整树复制」与 GM-10A 可维护性。
3. 空 `tests/README.md` 使来源/许可边界不足以支撑生产测试树落地。

启用分支：`GM-10B`（DEFER-GM-03 handoff）；取消：`GM-10A`。

## Minimal allowlist

NO-GO：无启用中的逐文件 allowlist。  
（若未来改为 GO，必须先补 regenerate 文档、tip→object 闭包的小体积 allowlist，并重新 GM-09 review。）

## Handoff

- 接收卡：`GM-10B`
- 延后项：`DEFER-GM-03`（由 GM-10B 从 `inactive-reserve` 升为 `active`）
- 接收方：`plan-long.md` PT-03/PT-04 follow-up
- 重启条件：许可/provenance 澄清，或对象层差分需求使小体积 tip 闭包可独立再生
