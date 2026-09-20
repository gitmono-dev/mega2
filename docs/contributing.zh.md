[English](contributing.md) · 中文

# 贡献指南

本文是向 mega2 提交代码的入口文档：流程、门禁、约定与常见任务 how-to。事实基线是
当前 checkout 的源码与 [`../AGENTS.md`](../AGENTS.md)；二者冲突时以 `AGENTS.md`
与源码为准，并提 Issue 修正本文。

## 1. 贡献流程

不要冷启动大改动。顺序是：

1. **先开 Issue。** 写清问题、动机、范围与明确的非目标（non-goals）。等维护者
   （或讨论）认可方向后再往下走。
2. **再写计划。** 从 [`plan/plan-template.md`](plan/plan-template.md)（中文规范原文）
   或 [`plan/plan-template.en.md`](plan/plan-template.en.md)（English contributor
   edition）复制结构，落盘为 `docs/plan/plan-YYYYMMDD.md`。强制章节不得删除，
   不适用时写 `N/A` 并说明原因。计划规则（命名、事实基线、任务卡、索引登记）
   见 [`plan/README.md`](plan/README.md)。
3. **计划过审后才实现。** 把工作拆成可独立执行的任务卡（范围 / 依赖 / 文件落点 /
   验收标准 / 验证命令明确），补测试与文档，过第 3 节的三门禁后合入。

**计划不等于实现。** 落笔时的事实基线是当前 checkout 的源码、测试、配置与文档；
历史计划与 Issue 里的口头共识只作线索，不能替代任务卡上的验证命令。

## 2. 开发环境

环境准备、compose 数据面、集成测试栈与故障排查全部以
[`development.md`](development.md) 为准，本文不复制。优先用统一入口脚本
[`../scripts/dev-test.sh`](../scripts/dev-test.sh)（`up-full` / `basic` /
`full` / `gates` 等），避免手贴命令漏步骤；共享逻辑在
[`../scripts/lib/mega2-it.sh`](../scripts/lib/mega2-it.sh)。测试 env 模板是
[`../.env.test.example`](../.env.test.example)（`dev-test.sh` 会自动生成本地
`.env.test`，不提交）。

## 3. 提交前三门禁

任何代码改动提交前必须三门全绿（与 [`../AGENTS.md`](../AGENTS.md) 一致；等价
封装：`./scripts/dev-test.sh gates`）：

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
source .env.test && cargo test --all
```

要求：fmt 无 diff（nightly  toolchain，因 `rustfmt.toml` 可能启用不稳定选项）；
clippy 0 warning 0 error，不得用 blanket `#[allow(...)]` 绕过；测试全过，
不得通过 `#[ignore]` / 删断言来变绿。`.env.test` 缺失时先按
[`development.md`](development.md) 生成，不要跳过 source。

## 4. 代码约定（摘要）

完整约定见 [`../AGENTS.md`](../AGENTS.md) 的 Code Conventions 与 Common Pitfalls
两节，此处只列最常踩的几条：

- **导入分组**：`std` → 外部 crate → `crate::`；库代码不用 `use crate::*` 通配
  （`mod tests` 内可以）。
- **错误类型**：按模块现状用 `MegaError` / `MegaResult`、`anyhow::Result` 或
  `thiserror` 之一，同一模块内不混用。
- **日志**：用 `tracing::{info, warn, error, debug, trace}` 宏，不用 `println!`；
  优先结构化字段。
- **DB 访问**：走 `src/jupiter/storage/` 的 `*Storage` 类型，不在 API / handler
  里直接调 `sea_orm`。
- **依赖**：新增 `Cargo.toml` 依赖先论证必要性（编译时间 / 体积 / 许可），优先
  复用已有 vendored 依赖。
- **分配器**：不动 `src/main.rs` 的 `#[global_allocator]` 块，除非就是要在该
  平台换分配器。
- **注释**：稀疏、英文，密度与周边文件一致。
- 注意没有顶层 `mod vault`：Vault 类型从 `libvault::*` 与
  `crate::contract::vault::*` 引入（细节见 AGENTS.md 的 Pitfalls）。

## 5. 常见任务 how-to

**新增 CLI 子命令**（步骤细节见 [`../AGENTS.md`](../AGENTS.md)「Adding a New
Subcommand」）：

1. 在 `src/commands/<name>.rs` 实现命令模块。
2. 在 `src/commands/mod.rs` 的 `builtin()` 注册 clap `Command`，并在
   `builtin_exec()` 接线 executor（签名 `fn(config: Config, args: &ArgMatches) -> MegaResult`）。
3. 在命令旁加单测，并在 `src/cli.rs::tests` 仿照现有用例加 CLI 解析测试。

**新增 DB 实体 / 迁移**（步骤细节见 [`../AGENTS.md`](../AGENTS.md)「Adding a
New DB Entity / Migration」）：

1. 实体文件放 `src/callisto/<table>.rs` 并登记进 `src/callisto/mod.rs`。
2. migrator 放 `src/jupiter/migration/` 并注册进该模块的 migrator 列表。
3. 需要新领域存储时，在 `src/jupiter/storage/` 加 `<domain>_storage.rs` 并从
   `storage/mod.rs` re-export。
4. 用 `crate::jupiter::tests::test_db_connection` +
   `crate::jupiter::migration::apply_migrations` 写 `#[cfg(test)]` 覆盖
   （范例见 `notification/dispatcher.rs::tests`）。

## 6. 文档约定

- **计划文档**：强制模板、命名、事实基线、任务卡可执行性、索引与状态同步等
  规则见 [`plan/README.md`](plan/README.md)，不自创格式。
- **事实基线**：文档只陈述当前 checkout 可验证的事实；计划文档不宣称实现完成。
- **链接而非复制**：配置键表、token 值、命令 flag 列表等有权威出处的内容
  （[`../config/config.toml`](../config/config.toml)、
  [`refactoring/config.md`](refactoring/config.md)、[`deploy-trunk.md`](deploy-trunk.md)、
  [`monorepo.md`](monorepo.md)、[`development.md`](development.md)、
  [`../AGENTS.md`](../AGENTS.md)）一律链接，不在新文档里复制数值。
- **中英双语**：英文是默认文件（如 `foo.md`），中文放在同名的 `.zh.md` 旁
  （如 `foo.zh.md`），两版结构一致、互为翻译；文件顶部放语言切换行（与
  [`../README.md`](../README.md) 同款）。本文即按此约定维护。
- 文档中的相对链接必须指向当前 checkout 里存在的文件，提交前逐一确认。

## 7. 版本控制：Libra 与任务卡发布

本仓 VCS 是 **Libra**（不是 git；无 `.git` 目录），命令为 `libra add` /
`libra commit` / `libra push` 等。交互式浏览 monorepo 用 Libra 的
`libra mega2 browser`。

任务卡收口发布流程（详见 [`../AGENTS.md`](../AGENTS.md)「Task card release」）：

1. 任务卡完成（Lifecycle=done、双 review PASS）后，按卡上的 `Version increment`
   （默认 patch +1）bump `Cargo.toml` `version`，并刷新 `Cargo.lock` 中的
   `mega2` 条目。
2. `libra add` + `libra commit -m`，**只提交该卡**的改动。
3. `libra push origin main`。**永远不要 `--force`**；分支与 origin 分叉时停下
   并报告，不自行处理。
4. 上一卡 commit 与 push 成功后才开下一卡。

## 相关文档

- 本套文档：[`quick-start.zh.md`](quick-start.zh.md) ·
  [`user-guide.zh.md`](user-guide.zh.md) · [`configuration.zh.md`](configuration.zh.md) ·
  [`deployment.zh.md`](deployment.zh.md) · [`architecture.zh.md`](architecture.zh.md)
- [`../AGENTS.md`](../AGENTS.md) — 门禁、代码约定、常见陷阱、任务卡发布的权威出处
- [`development.md`](development.md) — 本地开发与测试
- [`plan/README.md`](plan/README.md) — 计划文档规则
- [`../README.md`](../README.md) — 项目总览（Contributing 一节是本文的英文摘要）
