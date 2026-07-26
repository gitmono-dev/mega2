# monoengine 计划模板

本文是 `docs/plan/` 下新建计划的标准模板。新计划应复制本文件结构，替换 `<...>` 占位符，并删除不适用的说明性文字；强制章节不得删除，不适用时写 `N/A` 和原因。

## 使用规则

- 日期计划命名为 `plan-YYYYMMDD.md`，用于可执行的实现、迁移、重构或发布任务。
- 长期能力只进入 `plan-long.md`。日期计划可以链接长期能力编号，但不得把长期路线图复制成重复任务表。
- 每个计划必须以当前 checkout 的源码、测试、配置和文档为事实基线。历史计划、截图、会议记录只能作为线索；Mega 是移植目标项目，其 pinned revision 的当前源码是移植基线，但对 Mega 的历史描述同样不能作为已实现证据。
- 每个任务卡必须能交给 Agent 独立执行：范围明确、依赖明确、文件落点明确、验收标准明确、验证命令明确。
- 涉及公开命令、配置项、DB schema、HTTP API、错误码、存储格式、迁移、权限或安全边界的计划，必须包含测试、文档、回滚和兼容处理。
- 若计划引用 Mega 目标项目或外部项目（如 Libra、orbit 上游）作为参照，必须 pin 具体 revision、文件路径和核对日期；不得把浮动 `main` 当作规范。Mega 是移植目标而非竞品，其当前 pinned 源码即移植基线。
- 新增或修改 entity / storage / migration 时，必须同步 `src/callisto/`、`src/jupiter/storage/`、`src/jupiter/migration/`，并补对应集成测试。
- 生产代码不得新增未解释的 `unwrap()`、`expect()` 或 `panic!()`；如确属不可失败逻辑，必须有 `// INVARIANT:` 注释并在任务验收中说明。

## 标题

`# <主题>计划（<YYYY-MM-DD>）`

## 文档职责

本文解决 `<问题/能力>`，目标是 `<可交付结果>`。

本文只规划任务，不宣称实现完成。落地时每个任务都必须先刷新源码锚点，再按任务卡验收。

### 适用范围

- `<包含的命令/模块/服务>`
- `<包含的 DB schema、HTTP API、配置项或存储格式>`
- `<包含的测试、文档、迁移或发布动作>`

### 非目标

- `<明确不做的能力>`
- `<延后到其它计划/RFC/ADR 的范围>`
- `<容易被误解但本计划不承诺的行为>`

### 成功定义

- `<用户或系统行为变化>`
- `<机器接口或数据状态变化>`
- `<文档、测试、发布证据>`
- `<何时可标记计划完成>`

## 事实基线

> 所有行号和源码锚点必须在开工当天刷新。过期锚点只能作为历史线索。

| 类别 | 当前事实 | 证据 |
|---|---|---|
| 代码入口 | `<src/...>` | `<file:line>` |
| 数据/状态 | `<DB table / redis key / object path>` | `<file:line>` |
| CLI 命令 | `<monoengine ...>` | `<src/commands/mod.rs:line>` |
| HTTP API | `<METHOD /api/...>` | `<src/api/...:line>` |
| 配置项 | `[section].key` | `<src/common/config/...:line>` |
| 错误类型 | `<MegaError::...>` | `<src/common/errors.rs:line>` |
| 文档 | `<docs/...>` | `<file:line>` |
| 测试 | `<tests/... 或 mod tests>` | `<module::test_fn>` |
| 外部参照 | `<repo@sha>` | `<path + date>` |

### 当前缺口

| ID | 缺口 | 影响 | 证据 | 计划动作 |
|---|---|---|---|---|
| GAP-01 | `<问题>` | `<用户/生产影响>` | `<file:line 或外部证据>` | `<任务 ID>` |

## 与其它计划的关系

| 计划/文档 | 关系 | 本计划处理 |
|---|---|---|
| `plan-long.md` | `<关联 LR/SB/UP 编号>` | `<链接、消费、更新状态或不触碰>` |
| `plan-YYYYMMDD.md` | `<前置/并行/替代/冲突>` | `<复用、不重做、迁移、关闭>` |
| `docs/refactoring/*.md` | `<事实源或契约>` | `<同步方式>` |
| `AGENTS.md` | `<工程约束基线>` | `<遵守或提出修订>` |

## 评审结论与修订记录

计划成稿前必须从以下维度做一次自审；如果有阻断项，先修计划再开工。

| 维度 | 结论 | 修订动作 |
|---|---|---|
| 合理性 | `<目标是否值得做>` | `<调整>` |
| 可行性 | `<任务是否可拆、可交付>` | `<调整>` |
| 完整性 | `<测试/文档/迁移/回滚是否齐全>` | `<调整>` |
| 安全性 | `<权限、secret、路径、网络、模型输入>` | `<调整>` |
| 功能正确性 | `<状态机、边界条件、错误路径>` | `<调整>` |
| 接口兼容 | `<CLI/HTTP API/配置/schema/错误>` | `<调整>` |
| 数据流与控制流 | `<事务、幂等、并发、分布式状态>` | `<调整>` |
| 性能与容量 | `<热路径、复杂度、存储增长>` | `<调整>` |
| 可靠性与容错 | `<崩溃恢复、重试、资源释放>` | `<调整>` |
| 可维护性 | `<事实源、抽象边界、重复实现>` | `<调整>` |

## 已决议设计决策

实现时若需偏离本节，必须先修改计划并说明原因，不得在代码中静默改语义。

### ADR-<PREFIX>-01: <决策标题>

- **Status:** Accepted
- **Context:** `<为什么需要这个决策>`
- **Decision:** `<选定方案>`
- **Alternatives considered:** `<备选方案及拒绝理由>`
- **Consequences:** `<带来的约束、风险、后续工作>`
- **Revisit when:** `<何时应重审>`

## 全局工程约束

以下约束对本文所有任务生效。任务条目不再逐条重复，违反任一项即视为任务未完成。

- **GC-01 现状核实前置:** 每个任务开工前重新核对计划、相关开发文档、当前代码和测试。如果已实现，则任务改为补测试、补文档、更新状态或关闭，不重复实现。
- **GC-02 单一事实源:** entity 定义、配置解析、API schema、权限策略、错误码和共享 helper 必须有单一事实源。禁止 CLI handler、HTTP handler、migration 和测试 fixture 各自复制等价逻辑。
- **GC-03 Mega 目标项目与 monoengine 扩展边界:** 直接从 Mega 移植的代码表面必须标注来源和差异；monoengine-only 表面必须说明替代方案、用户影响和机器接口。
- **GC-04 输出与错误契约:** 用户可见错误使用 `MegaError` 稳定变体并同步 `docs/errors.md`。HTTP 状态码、JSON 响应、CLI 退出码和人读输出必须分别验收。
- **GC-05 文档同步:** 命令、配置、HTTP API 或公开行为变化必须同步对应 `docs/` 下文档和 OpenAPI schema。
- **GC-06 测试覆盖:** 新增 entity / storage / migration 必须附带 `#[cfg(test)] mod tests`，使用 `test_db_connection` + `apply_migrations` 集成测试。新增 CLI 子命令必须附解析测试。
- **GC-07 安全默认值:** 未满足认证、授权（Cedar）、路径归属、schema 版本、对象闭包或 secret redaction 前置时默认 fail-closed。任何 fail-open 必须有显式用户选择、日志和测试。
- **GC-08 原子性与恢复:** 修改 DB 事务、redis 状态、对象存储、配置、vault secret 或发布状态时，必须定义事务边界、幂等键、崩溃窗口和回滚/前滚策略。
- **GC-09 并发与资源生命周期:** DB 连接池、redis 连接、文件句柄、异步任务队列和临时目录必须有释放/恢复语义；测试不得依赖未隔离的全局状态。
- **GC-10 性能预算:** HTTP 热路径、DB 查询、对象存储读写、Git 协议操作和后台任务不得引入无界扫描、无界内存或 N+1 DB/网络调用。需要时写出数据规模和断言。
- **GC-11 生产 panic 禁止:** 生产路径不得新增裸 `unwrap()`、`expect()`、`panic!()`；必须用 `MegaResult`、`anyhow::Context` 或领域错误返回可操作信息。
- **GC-12 精确提交:** 提交前只 `libra add <相关路径>`（本仓由 libra 管理，无 `.git`，`git add` 不可用），不得使用 `commit -a`。发现无关脏状态时保留并报告，不得清理、重置或混入提交。

## 执行检查必备需求（强制）

任一要求未满足，对应任务不得标记完成。

1. **开工前安全检查:** 必须使用 libra 运行 `libra status --short`，确认当前分支、工作区脏状态和目标文件是否已有无关改动。若目标文件已有未确认用户改动，先报告并避免覆盖。
2. **先核对后实现:** 刷新本任务相关源码锚点、文档锚点、测试 target 和外部参照 revision，再决定实现、补测、补文档、关闭或降级。
3. **每个任务三门验收:** 至少通过 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、任务卡指定的 `source .env.test && cargo test ...`。不能用泛泛的 `cargo test` 替代指定用例；指定用例之外还必须通过全量门禁 `source .env.test && cargo test --all`（`AGENTS.md` 强制），指定用例与全量门禁不可互相替代。
4. **代码 review 闭环:** 实现和本地验收完成后进行代码 review；review 问题修复后重跑相关验收，直到 review 明确通过。
5. **文档与兼容同步:** 涉及公开行为的任务必须同步用户文档、开发文档、OpenAPI schema、配置示例、错误码和测试。
6. **构建不变量:** `cargo build` 和 `cargo build --tests` 必须 0 错误 0 警告，且不得新增 crate 级 `#[allow(...)]`。
7. **版本与发布:** 若任务发布用户可见代码改动，按开工时实际版本号 patch +1，同步 `Cargo.toml`，并记录构建/发布证据。纯文档计划可写 `N/A`，但必须说明原因。
8. **push 失败策略:** 非 fast-forward 需要 pull/merge 后重新验收再推；认证、权限、网络或服务端失败不 blind retry，记录原因，待下一次修复/发布窗口处理。
9. **内部服务错误:** Redis、Postgres、对象存储、SMTP、AI provider 等暂时性错误不得直接把任务宣告完成。记录错误并在同一步重试；确定性代码或配置错误转为当前任务修复项。
10. **证据卫生:** 验收证据不得保存 secret、API key、token、PII、未脱敏 transcript、绝对私有路径或原始 tool payload。需要留存时只写 sanitized summary。

## 实施顺序

依赖边格式：`A -> B` 表示 A 必须先于 B。

- `<TASK-01> -> <TASK-02>`
- `<TASK-02> -> <TASK-03>`

### Phase 0: <基线冻结和消歧>

**目标:** `<本阶段目标>`

**进入条件:**

- `<前置条件>`

**退出条件:**

- `<阶段完成判据>`

### Phase 1: <实现第一个可发布切片>

**目标:** `<本阶段目标>`

**进入条件:**

- `<前置条件>`

**退出条件:**

- `<阶段完成判据>`

## 任务卡

任务 ID 使用稳定前缀，例如 `A0-01`、`DR-01`、`W1-02`、`P0-03`。编号被引用后不重排，废弃时保留并标记替代关系。

### Task <ID>: <任务标题>

**Description:** `<要做什么、为什么、现实影响。必须写清不做什么。>`

**Current evidence:**

| 事实 | 证据 |
|---|---|
| `<当前实现或缺口>` | `<file:line / test / external repo@sha>` |

**Acceptance criteria:**

- [ ] `<用户可见或系统行为判据>`
- [ ] `<API/配置/schema/错误码判据>`
- [ ] `<失败路径/边界条件判据>`
- [ ] `<文档/兼容/迁移同步判据>`

**Verification:**

- [ ] `<exact command>`
- [ ] `<exact command>`
- [ ] `<manual/sanitized evidence, if required>`

**Dependencies:** `<无 / Task ID 列表 / 外部前置>`

**Files likely touched:** `<src/...>, <tests/...>, <docs/...>, <config/...>`

**Docs and compatibility impact:** `<N/A 或具体文件>`

**Migration and rollback:** `<N/A 或 sea-orm migration up/down、配置回滚、数据前滚策略>`

**Security and privacy:** `<N/A 或 Cedar 策略、secret、路径、redaction、输入校验约束>`

**Performance budget:** `<N/A 或数据规模、复杂度、wall-clock/benchmark 断言>`

**Estimated scope:** `<S/M/L/XL>`

**Release boundary:** `<独立发布 / 随 Phase N 发布 / 文档-only N/A>`

## 测试矩阵

| 类别 | 必须覆盖 | Target / command |
|---|---|---|
| 单元 | `<纯逻辑、config parser、错误映射>` | `<cargo test <module>>` |
| 集成 | `<真实 DB + storage 工作流>` | `<cargo test -- <test_fn>>` |
| CLI | `<子命令解析、退出码、输出>` | `<cargo test cli::tests::...>` |
| HTTP API | `<路由、状态码、JSON schema、鉴权>` | `<cargo test api::...>` |
| 迁移 | `<up/down、old/new schema、数据迁移>` | `<cargo test migration::...>` |
| 安全 | `<鉴权、Cedar 策略、secret、路径 traversal>` | `<cargo test ...>` |
| 性能 | `<规模与预算>` | `<criterion / wall-clock>` |
| live/gated | `<真实外部服务或 provider>` | `<feature/env gated command>` |

## 追溯表

| 任务 | 来源/证据 | monoengine 落点 | 文档/兼容动作 | 指定测试 |
|---|---|---|---|---|
| `<ID>` | `<file:line / issue / repo@sha>` | `<src/callisto / src/jupiter / src/api / ...>` | `<docs/...、config/config.toml、OpenAPI>` | `<module::test_fn>` |

## 里程碑验收与回滚

| 里程碑 | 完成条件 | 发布/证据 | 回滚或前滚 |
|---|---|---|---|
| M0 | `<基线冻结>` | `<commit/test/doc>` | `<N/A>` |
| M1 | `<首个可发布切片>` | `<version/test/review>` | `<rollback/forward fix>` |

### 故障恢复矩阵

| 故障点 | 可接受残留 | 恢复动作 | 禁止结果 |
|---|---|---|---|
| `<DB 事务中途、提交前>` | `<临时表/部分写入>` | `<retry/abandon/rollback>` | `<数据丢失/部分提交/静默成功>` |

## 风险登记

| 风险 | 影响 | 缓解 | 任务 |
|---|---|---|---|
| `<风险>` | `<高/中/低 + 影响>` | `<测试/设计/门禁>` | `<ID>` |

## 性能与容量摘要

| 操作 | 单次成本 | 累积成本 | 预算/上限 | 验证 |
|---|---|---|---|---|
| `<操作>` | `<O(...)>` | `<O(...)>` | `<阈值>` | `<测试/benchmark>` |

## 兼容与文档收口

- [ ] `docs/errors.md` 已同步，或说明 `N/A`。
- [ ] `docs/refactoring/*.md` 相关文档已同步，或说明 `N/A`。
- [ ] `config/config.toml` 示例配置已同步，或说明 `N/A`。
- [ ] OpenAPI / `utoipa` schema 已同步，或说明 `N/A`。
- [ ] `AGENTS.md` 相关工程约束已同步，或说明 `N/A`。
- [ ] `src/callisto/` 与 `src/jupiter/migration/` 已同步，或说明 `N/A`。
- [ ] `plan-long.md` 日期计划索引或 LR 状态已同步，或说明 `N/A`。

## Review log

| Round | Scope | Result | Required fixes | Evidence |
|---|---|---|---|---|
| R1 | `<files/tasks>` | `<PASS / issues>` | `<fix IDs>` | `<test commands>` |

## 非目标与延后项

| ID | 延后内容 | 原因 | 重启条件 | 承接位置 |
|---|---|---|---|---|
| DEFER-01 | `<内容>` | `<原因>` | `<何时重启>` | `<plan/RFC/ADR>` |

## 完成判据

计划只有在以下条件全部满足后才能标记完成：

- [ ] 所有非延后任务的 acceptance criteria 已满足。
- [ ] 所有任务的 Verification 命令已运行并记录结果。
- [ ] 必要的 docs/API/配置/迁移/测试更新已完成。
- [ ] 必要的 migration、rollback、failure-recovery 验证已完成。
- [ ] 代码 review 已通过，或 residual risk 已明确记录并被接受。
- [ ] 如有发布要求，版本、构建、安装、提交、推送和发布证据已完成。
- [ ] `plan-long.md` 相关状态或日期计划索引已同步，或明确 `N/A`。
