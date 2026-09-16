# mega2 计划模板

本文是 `docs/plan/` 下新建计划的标准模板。新计划应复制本文件结构，替换 `<...>` 占位符，并删除不适用的说明性文字；强制章节不得删除，不适用时写 `N/A` 和原因。英文贡献者使用 [`plan-template.en.md`](plan-template.en.md)；两份冲突时以本文为准。

**模板版本:** `v2.1`（2026-09-16 起生效；版本 bump 后必须创建同名 `v<version>` tag、发布人工编写的 GitHub Release note。其余结构与 `v2` 相同。）

### 模板版本与迁移政策

- 生效日期之后**新建**的计划必须整份符合本版模板。
- 生效日期之前成稿的计划（当前为 `plan-20260727.md`）按**增量迁移**：只有本次被新增或做规范性修改的任务卡需要满足本版 `G-*` 与新增字段；未触碰的卡保持原样，不构成违规，也不要求整份回填。
- 存量计划整份迁移是一次独立的计划工作，必须单独立卡；不得作为其它任务的附带产物。
- 若某份存量计划因迁移成本暂时保留与本版冲突的口径（例如旧的字段集、L/XL 卡），在该计划的「修订历史」登记一行例外与预期迁移时机即可。

## 使用规则

- 日期计划命名为 `plan-YYYYMMDD.md`，用于可执行的实现、迁移、重构或发布任务。
- 长期能力只进入 `plan-long.md`（当前使用 `PT-*` 与 `SB-*` 编号）。日期计划可以链接长期能力编号，但不得把长期路线图复制成重复任务表。
- 每个计划必须以当前 checkout 的源码、测试、配置和文档为事实基线。历史计划、截图、会议记录只能作为线索；Mega 是移植目标项目，其 pinned revision 的当前源码是移植基线，但对 Mega 的历史描述同样不能作为已实现证据。
- 每个任务卡必须能交给 Agent 独立执行：范围明确、依赖明确、文件落点明确、验收标准明确、验证命令明确。
- 每个任务卡必须满足「任务卡粒度规则」全部 `G-*` 条款：单一可独立恢复的行为轴、条目与规模在上限内、默认一张卡一个发布切片。粒度不合格的卡不得进入开工态，必须先拆分或合并。
- 涉及公开命令、配置项、DB schema、HTTP API、错误类型、存储格式、Git 协议、迁移、权限或安全边界的计划，必须包含测试、文档、回滚和兼容处理。
- 若计划引用 Mega 目标项目或外部项目（如 Libra、orbit 上游）作为参照，必须 pin 具体 revision、文件路径和核对日期；不得把浮动 `main` 当作规范。Mega 是移植目标而非竞品，其当前 pinned 源码即移植基线。
- 新增或修改 entity / storage / migration 时，必须同步 `src/callisto/`、`src/jupiter/storage/`、`src/jupiter/migration/`（含 `src/jupiter/migration/mod.rs` 的 `migrations()` 注册列表），并补对应集成测试。
- 引用 `docs/*.md` 路径前必须确认该文件存在。仓库中已存在多处指向不存在文档的引用，新计划不得继续制造悬空引用：要么在同卡内创建该文档，要么改引现存文档。
- 生产代码不得新增未解释的 `unwrap()`、`expect()` 或 `panic!()`；如确属不可失败逻辑，必须有 `// INVARIANT:` 注释并在任务验收中说明。

### 规范性 ID 与术语

计划正文引用规范条款时一律用具体 ID（例如「按 G-03 拆卡」），不要用「上一节」「前面那条」或会随条款增删失效的范围表述。新增条款时必须同步下表。

| 前缀 | 含义 | 定义位置 |
|---|---|---|
| `ER-*` | 执行检查必备需求（开工、验收、发布、证据） | 「执行检查必备需求」 |
| `GC-*` | 全局工程约束（对全部任务生效） | 「全局工程约束」 |
| `G-*` | 任务卡粒度规则 | 「任务卡粒度规则」 |
| `ADR-*` | 已决议设计决策 | 「已决议设计决策」 |
| `GAP-*` | 事实基线缺口 | 「当前缺口」 |
| `DEP-*` | 依赖登记项（含跨计划与外部前置） | 「依赖登记表」 |
| `REL-*` | 发布分组（含家族卡窗口） | 「发布分组与并发窗口」 |
| `EX-*` | 白名单内的规则 waiver（需具名审批） | 「字段全局默认与例外」 |
| `FIX-*` | 执行期发现的越界修复卡（ER-10） | 对应 Phase 末尾 |
| `DEFER-*` | 延后项（本仓惯例为 `DEFER-<计划前缀>-NN`） | 「非目标与延后项」 |
| `M<n>` | 里程碑（本仓惯例为 `M0`、`M1`…，无连字符） | 「里程碑验收与回滚」 |

跨文档已存在、本模板不得占用或改写的编号：`plan-long.md` 的 `PT-01..PT-13`（长期能力；`PT-13` 起可为 mega2 原生项）与 `SB-01..SB-03`（工程安全基线）；各日期计划自有的任务前缀（如 `plan-20260727.md` 的 `IT-*`）。

术语（全文统一，不要混用同义词）：

- **行为轴**：一个可独立恢复、对外语义自洽的变化方向（例如「LFS 批处理鉴权」是一个轴，「LFS 对象传输内容寻址」是另一个轴）。
- **落点**：一个可枚举的代码或文档归属域，粒度为**一个具体目录**（如 `src/jupiter/storage/`、`src/api/router/`）或**一组同主题文档**（如 `docs/refactoring/config.md`）。仓库根、`src/`、`tests/`、`docs/` 这类顶层目录**不算**一个落点。
- **写集**：会被修改的文件/目录集合，分三类（G-10）——**实现写集 I**（每卡字段，决定能否并发）、**发布写集 R**（每卡字段，版本面 + `Cargo.lock`；当前版本面只有一处，即 `Cargo.toml` 的 `version`，见 ER-08；不用于实现阶段的并发分组，但进入发布窗口后按 I–R / R–R 规则串行化）、**协调写集 C**（计划级，发布顺序与窗口记录，不进任务卡字段、不参与并发判定；「禁止多 Agent 并发发布」是 ER-12 的仓库级规则，不是 C 的状态）。
- **发布切片**：一次独立的 review + 验收 + 版本 + 提交 + 推送。
- **家族卡**：共用唯一发布点的一组子卡（G-08）。
- **恢复模式（字段名 `Rollback mode`）**：`revert` / `forward-only` / `compensating` / `immutable-release` 四种之一（G-01）。不可逆变更用后三种表达，不要求「一次 revert 撤销」。

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
| 数据/状态 | `<Postgres 表 / redis key / object namespace>` | `<file:line>` |
| CLI 命令 | `<monoengine ...>` | `<src/commands/mod.rs:line（builtin / builtin_exec / load_mode 三处注册）>` |
| HTTP API | `<METHOD /api/v1/...>` | `<src/api/router/...:line>` |
| 配置项 | `[section].key` | `<config/config.toml:line + src/config/model.rs:line>` |
| 错误类型 | `<MegaError::...>` | `<src/common/errors/mod.rs:line>` |
| 迁移 | `<m<YYYYMMDD>_<HHMMSS>_<slug>>` | `<src/jupiter/migration/mod.rs 的 migrations() 注册行>` |
| 文档 | `<docs/...>` | `<file:line>` |
| 测试 | `<-p monoengine --lib '<mod::tests>' 或 -p monoengine --test <target>>` | `<file:line>` |
| 工作区前置 | `<.env.test 是否存在 / 测试栈是否已起（Postgres 15432、Redis 16379…）>` | `<.env.test.example / docker-compose.test.yml:line>` |
| 外部参照 | `<Mega repo@sha>` | `<path + 核对日期>` |

### 当前缺口

| ID | 缺口 | 影响 | 证据 | 计划动作 |
|---|---|---|---|---|
| GAP-01 | `<问题>` | `<用户/生产影响>` | `<file:line 或外部证据>` | `<任务 ID>` |

## 与其它计划的关系

| 计划/文档 | 关系 | 本计划处理 |
|---|---|---|
| `plan-long.md` | `<关联 PT/SB 编号>` | `<链接、消费、更新状态或不触碰>` |
| `plan-YYYYMMDD.md` | `<前置/并行/替代/冲突>` | `<复用、不重做、迁移、关闭>` |
| `docs/refactoring/*.md` | `<事实源或契约>` | `<同步方式>` |
| `AGENTS.md` / `README.md` | `<工程约束基线>` | `<遵守、提出修订或登记漂移>` |

## 评审结论与修订记录

计划成稿前必须从以下维度做一次自审；如果有阻断项，先修计划再开工。

| 维度 | 结论 | 修订动作 |
|---|---|---|
| 合理性 | `<目标是否值得做>` | `<调整>` |
| 可行性 | `<任务是否可拆、可交付>` | `<调整>` |
| 任务卡粒度 | `<是否存在多轴卡、L/XL 卡、碎片卡、未登记的合并发布>` | `<按 G-* 拆分/合并/登记例外>` |
| 依赖与顺序 | `<DAG 是否无环、是否缺边、发布顺序是否可执行>` | `<调整>` |
| 完整性 | `<测试/文档/迁移/回滚是否齐全>` | `<调整>` |
| 安全性 | `<权限、secret、路径、网络、模型输入>` | `<调整>` |
| 功能正确性 | `<状态机、边界条件、错误路径>` | `<调整>` |
| 接口兼容 | `<CLI/HTTP API/配置/schema/错误>` | `<调整>` |
| 数据流与控制流 | `<事务、幂等、并发、分布式状态>` | `<调整>` |
| 性能与容量 | `<热路径、复杂度、存储增长>` | `<调整>` |
| 可靠性与容错 | `<崩溃恢复、重试、资源释放>` | `<调整>` |
| 可维护性 | `<事实源、抽象边界、重复实现>` | `<调整>` |

### 修订历史

计划成稿后的每次规范性变更（任务卡拆分/合并、依赖调整、发布边界变化、决策反转）都必须在此登记一行；G-09 的拆分同步以本表为闭环凭证。

| 日期 | 触发 | 变更内容 | 原卡 → 新卡 | 受影响的引用 |
|---|---|---|---|---|
| `<YYYY-MM-DD>` | `<自审 / review R<n> / 现状核对>` | `<做了什么规范性修改>` | `<TASK-ID> → <TASK-ID>, <TASK-ID>` | `<实施顺序、依赖登记表、REL-*、追溯表、测试矩阵、里程碑、风险表>` |

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
- **GC-02 单一事实源:** entity 定义、配置解析、API schema、权限策略、错误类型和共享 helper 必须有单一事实源。禁止 CLI handler、HTTP handler、migration 和测试 fixture 各自复制等价逻辑。
- **GC-03 Mega 目标项目与 monoengine 扩展边界:** 直接从 Mega 移植的代码表面必须标注来源和差异；monoengine-only 表面必须说明替代方案、用户影响和机器接口。
- **GC-04 输出与错误契约:** 用户可见错误使用 `MegaError` 稳定变体并同步 `docs/errors.md`。HTTP 状态码、JSON 响应、CLI 退出码和人读输出必须分别验收。注意 `MegaError` 目前**没有**数值错误码注册表，变体→HTTP 状态的映射只存在于 `src/common/errors/api.rs` 的转换实现里；改动该映射必须同时更新 `docs/errors.md` 或在卡内说明为何不需要。
- **GC-05 文档同步:** 命令、配置、HTTP API 或公开行为变化必须同步对应 `docs/` 下文档、`config/config.toml` 注释与 `README.md`。本仓 `docs/` 为中文单语，无 EN/zh 双份要求；OpenAPI 由 `utoipa` 在运行时聚合、**磁盘上没有落盘的 spec 文件**，因此 API schema 证据只能取自运行中的 `/api/openapi.json`。
- **GC-06 测试覆盖:** 新增 entity / storage / migration 必须附带 `#[cfg(test)] mod tests`，使用 `test_db_connection` + `apply_migrations` 集成测试。新增 CLI 子命令必须附解析测试，并覆盖 `builtin()` / `builtin_exec()` / `load_mode()` 三处注册。新增集成 test target 直接在 `tests/<name>.rs` 建文件（cargo 自动发现，无需 `[[test]]` 声明），但必须同步本计划「测试矩阵」与 `docs/refactoring/integration.md` 的覆盖矩阵。
- **GC-07 安全默认值:** 未满足认证、授权（Cedar）、路径归属、schema 版本、对象闭包或 secret redaction 前置时默认 fail-closed。任何 fail-open 必须有显式用户选择、日志和测试。已知现状：Cedar guard 当前使用硬编码 permit-all 且 `EntityStore` 启动时为空，任何依赖「授权已生效」的验收判据都必须先验证该前提，不得假定。
- **GC-08 原子性与恢复:** 修改 DB 事务、redis 状态、对象存储、配置、vault secret 或发布状态时，必须定义事务边界、幂等键、崩溃窗口和回滚/前滚策略。
- **GC-09 并发与资源生命周期:** DB 连接池、redis 连接、文件句柄、异步任务队列和临时目录必须有释放/恢复语义；测试不得依赖未隔离的全局状态（改环境变量的测试必须使用 `src/config/testing.rs` 的 `env_lock` / `EnvVarGuard`）。
- **GC-10 性能预算:** HTTP 热路径、DB 查询、对象存储读写、Git 协议操作和后台任务不得引入无界扫描、无界内存或 N+1 DB/网络调用。需要时写出数据规模和断言。
- **GC-11 生产 panic 禁止:** 生产路径不得新增裸 `unwrap()`、`expect()`、`panic!()`；必须用 `MegaResult`、`anyhow::Context` 或领域错误返回可操作信息。
- **GC-12 精确提交:** 提交前只 `git add <相关路径>`，不得使用 `commit -a`。发现无关脏状态时保留并报告，不得清理、重置或混入提交。（**2026-08-27 起本仓由 Git 管理**，`.libra` 已删除；此前计划中的 `libra add` / 「本仓无 `.git`」表述是当时的事实记录，按「模板版本与迁移政策」不追认为违规，也不回改历史计划。）

## 执行检查必备需求（强制）

任一要求未满足，对应任务不得标记完成。条目使用稳定 ID，正文引用时用 ID 而不是序号，便于后续插入条目而不破坏交叉引用。

1. **ER-01 开工前安全检查:** 必须完成下列四项，缺一不可。
   - `git status --short --branch`（`--branch` 才会输出 `## <branch>...<upstream>` 行；不带它只有文件状态），确认当前分支与计划指定分支一致、工作区脏状态、目标文件是否已有无关改动。若目标文件已有未确认用户改动，先报告并避免覆盖。已知现状（2026-08-27 核对）：`main` **未配置 upstream**（`git rev-parse --abbrev-ref main@{upstream}` 报 `no upstream configured`），因此 status 的 `## main` 行不带 ahead/behind，**不要**把它当作推送判据；需要判断领先/落后时显式用 `git rev-list --left-right --count origin/main...main`。
   - ~~确认路径依赖 `../orbit` 已就位~~ **（2026-08-27 订正：本项已失效，无需执行。）** 对象存储自 `plan-20260824` 起完全内联为 `src/orbit_api/`（traits/config/errors）与 `src/orbit/`（`object_store` 后端），由 `src/jupiter/storage/object_storage.rs::build_object_storage` 直接调用 `crate::orbit::factory::ObjectStorageFactory::build`；**已无** sibling `../orbit`、**已无** `crates/orbit*` workspace 成员、**已无** `orbit-api` path 依赖，也**没有** `ObjectStorageProvider` 进程级注册表。因此不存在「缺失 sibling checkout 导致 cargo 依赖解析失败」这一失败模式，遇到 `cargo` 解析失败应按真实原因排查。拓扑说明见 `docs/refactoring/orbit.md` 头部与 `README.md`「目录关系」。
   - 确认 `.env.test` 是否存在（仓库只提供 `.env.test.example`，`.env.test` 本身被忽略）。缺失时按 `AGENTS.md` 的规定停下来确认，**不得**静默降级为不 source 的 `cargo test --all`。
   - 确认需要的测试服务是否已启动：`docker compose -f docker-compose.test.yml up -d --wait`（Postgres `15432`、Redis `16379`、Mailpit `11025/18025`、RustFS `19000/19001`；RustFS 桶初始化需要额外的 `--profile init run --rm rustfs-init`）。Postgres 缺失会让相关用例直接 panic 而不是跳过。
2. **ER-02 先核对后实现:** 刷新本任务相关源码锚点、文档锚点、测试 target 和外部参照 revision，再决定实现、补测、补文档、关闭或降级。
3. **ER-03 粒度门禁:** 开工前按粒度规则 `G-*` 逐条复核本任务卡，并逐字段核对该卡的 `Granularity` 摘要行。若核对后发现范围已扩大（新增行为轴、AC/Verification 超限、scope 升到 L、写集与其它在跑任务重叠），先修改计划拆卡再开工，不得在实现中静默扩张任务范围。
4. **ER-04 每卡验收门:** 门由 **A 表面 focused 门**（按实际改动的表面）+ **B 类型门**（按 `Task type`）+ **C 发布收口门**（覆盖要求对所有非延后卡生效，执行归属见下）+ **D 远端后置门**（有不可本地复现的 CI 语义时）四组组成，**所有适用行累加**，全部通过才算验收。权威口径分层：`AGENTS.md`「Required Checks Before Submitting Code Changes」的三门（`cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test --all`）是**任何会提交的改动**的完成契约，模板不得削弱；`AGENTS.md` 另强制 `cargo build` 与 `cargo build --tests` 均 0 错误 0 警告。任务卡指定的 focused 用例是在此之上的**附加**门，用来证明本卡行为，不是三门的替代品。权威口径变更时必须同批同步 `AGENTS.md`、`docs/plan/README.md`，以及**采用本模板当前版本的计划**（存量计划按「模板版本与迁移政策」处理，不因本条被追认为违规）。

   **门不计入条目上限：** 本条列出的门是全局强制门，**不计入**任务卡 `Verification` 的 G-03 条目计数；`Verification` 只登记本卡特有的判据（指定用例、新增守卫、手工证据）。

   **两个正交状态字段（都在任务卡登记）:**
   - `Lifecycle`（执行生命周期）：`pending` | `in-progress` | `blocked`（ER-10 的越界故障置此值）| `done`。
   - `Acceptance`（验收状态）：空 | `locally-accepted` | `remote-pending`（仅当本卡有适用的 D 组远端后置门）| `complete`。与 `AGENTS.md` 的完成契约对齐 —— 任何改动只有三门全绿才算 done，因此：
     - `locally-accepted` = 本卡适用的 **A 组 + B 组**门已过，但本卡的 **C 组覆盖**（自行执行或从承接卡继承，见下）尚未取得。此状态下**不得**对外报告「完成 / done」。
     - `remote-pending` = A/B 已过且 C 组覆盖已取得（含一次三门全绿的运行，其被测树状态包含本卡最终变更），但本卡适用或继承的 **D 组**远端后置门尚未全绿。此状态同样**不得**报告完成。
     - `complete` = 本卡的 **A + B** 已过、**C 组覆盖**已取得、且适用或继承的 **D 组**已全绿。无 D 时 C 覆盖到手即可 `complete`；有 D（含继承的 D）时必须先经 `remote-pending`。
     - 唯一状态转移路径：A/B 通过 → `locally-accepted` → ER-05 review PASS → 取得 C 组覆盖 →（无 D：`complete`；有 D：`remote-pending` → D 全绿 → `complete`）。
   - 两者独立取值：`blocked` 卡的 `Acceptance` 可以已是 `locally-accepted` 甚至 `complete`（例如变更已被三门覆盖，但仍卡在外部前置）。`Lifecycle=done` 必须以 `Acceptance=complete` 为前提；`blocked` 必须先回到 `in-progress` 并完成剩余动作才能进入 `done`，**不允许**从 `blocked` 直接标 `done`。计划完成门另要求所有非延后任务都到 `done`（见「完成判据」）。`Granularity` 里的 `complete=yes` 是 G-02 的结构完整性判据，与本字段无关，不可混用。

   **C 组覆盖与执行归属（每张非延后卡都必须取得 C 覆盖，但不都自己执行）:**
   - **独立发布卡（`Release boundary = independent`）与发布点卡（`release` / `family release point`）**：自行执行完整 C 组门。
   - **`family child`**：不 bump、不构建发布产物、不推送，**继承**其家族唯一发布点的 C 覆盖——前提是该发布点的三门运行其被测树状态包含本子卡的最终变更；同时继承该发布点适用的 D 组。
   - **`no-release` 卡（`docs` / `audit` / `spike` / `handoff`）**：**继承**任务卡显式声明的承载发布点（或计划收口点）的 C 覆盖与其 D 组；该承载点必须在卡内写明 ID，不得留空。
   - 继承 D 组的卡同样要经过 `remote-pending`，直到被继承的 D 组证据全绿。
   - **不存在「用零命中守卫替代三门」的通道**——零命中守卫只用于证明这类卡未改代码，不改变其在取得 C 覆盖前仍是 `locally-accepted` 的事实。

   门分四组，全部适用者累加：**A 表面 focused 门**（按实际改动的表面，每个表面唯一命中一行，命中几行加几行）+ **B 类型门**（按 `Task type` 取一行）+ **C 发布收口门**（由会推送的卡执行，其覆盖可被 `family child` / `no-release` 卡继承）+ **D 远端后置门**（只在存在不可本地复现的 CI 语义时适用）。A/B/C 是本地可完成的门；D 只能在推送之后取得证据，因此**不阻塞** `locally-accepted` 与 ER-05 的 review 顺序。

   **A 表面 focused 门**（覆盖全部合法生产表面；`family child` 复用同一映射）：

   | 实际改动的表面 | focused 门 |
   |---|---|
   | lib target `monoengine_core` 的纯单元逻辑（测试写在 `src/**` 的 `#[cfg(test)]` 里） | `source .env.test && cargo test -p monoengine --lib '<mod::path::tests>'` |
   | lib target 中需要真实 Postgres 的逻辑（storage / entity / migration / config secret） | 先起测试栈，再 `source .env.test && cargo test -p monoengine --lib '<mod::path::tests>'`；用例必须走 `test_db_connection` + `apply_migrations` |
   | 进程级黑盒 / CLI 集成行为（`tests/**`） | `source .env.test && cargo test -p monoengine --test <target> -- --test-threads=1 [<filter>]`（`--test <target>` 不可省略：漏写会把 lib 单测一并拉进来，不再是 focused 门） |
   | bin target 源码（composition root：global allocator、`parse` 分发；本仓为 `src/main.rs` 与 `src/bin/migrate_local_to_s3.rs`） | `source .env.test && cargo test -p monoengine --test <target> -- --test-threads=1`（经 `CARGO_BIN_EXE_monoengine` 黑盒覆盖真实二进制）**加** `cargo clippy -p monoengine --all-targets -- -D warnings`（`--all-targets` 在单包形态下已覆盖两个 bin target，此处保留为本表面的 focused 证据） |
   | CLI 解析与注册（`src/cli.rs`、`src/commands/**`） | `cargo test -p monoengine --lib 'cli::tests'` + `cargo test -p monoengine --lib 'commands::'`；新增/改名子命令必须同时断言 `builtin()`、`builtin_exec()`、`load_mode()` 三处 |
   | HTTP API 路由 / handler / OpenAPI 注解（`src/api/**`、`src/server/http_server.rs`） | `cargo test -p monoengine --lib 'api::'` + 启动服务后拉取 `/api/openapi.json` 的 sanitized 证据（无落盘 spec，只能取运行时输出） |
   | `src/callisto/**`、`src/jupiter/migration/**` | `migrations()` 注册列表已登记的断言 + `apply_migrations(&db, true)` 集成用例；若该迁移的 `down` 是 no-op，必须在 `Rollback mode` 写 `forward-only`，不得声称可回滚 |
   | `config/config.toml`、`src/config/**` | config 校验链本地等价：`cargo run -p monoengine -- --config config/config.toml config validate`，按改动追加 `config init --output <tmp> --force`、`config validate --deny-warnings`、`--profile <name> config validate --show-sources`，以及坏配置的非零退出与 secret 不泄漏断言 |
   | Git 协议 / LFS（`src/ceres/protocol/**`、`src/contract/git_protocol/**`、`src/ceres/lfs/**`、`src/api/router/lfs_router.rs`、`src/server/http_server.rs`） | `scripts/git_protocol_smoke.sh` 本地等价，完整前置以 `.github/workflows/git-protocol-smoke.yml` 现场读取为准（本格只给骨架，不是快照事实源）：① `docker compose -f docker-compose.test.yml up -d --wait postgres redis`；② `cargo build --release -p monoengine`；③ **起 `service http` 前必须覆盖数据面**——仓库默认 `config/config.toml` 指向 `postgres://localhost:5432/...` 与 `redis://127.0.0.1:6379`，与测试栈的 `15432` / `16379` 不符，用默认配置起服务必然连不上；须 `source .env.test` 或显式导出 `MEGA_DATABASE__DB_URL`（建议独立 smoke 库）、`MEGA_REDIS__URL`、`MEGA_BASE_DIR`，并按 workflow 用同一 `MEGA_BASE_DIR` 预置 `mail.password` secret（mail 启动路径 fail-closed，缺 secret 时进程直接退出且不绑定端口）；④ 起服务后轮询 `/api/openapi.json` 就绪；⑤ 只读用例（ls-remote / clone / fetch / protocol v2 / shallow / blob:none）匿名即可通过（`git.anonymous_access` 默认 `true`），此时 `MONOENGINE_HTTP_REPO_URL=http://127.0.0.1:9000/` 足够；⑥ **push / tag / LFS 用例需要鉴权**：receive-pack 无有效 Mono access token 一律 401，而脚本只能通过 URL 传凭据，因此必须先向 smoke 库 `access_token` 表播种一次性 token，并写成 `http://<user>:<token>@127.0.0.1:9000/`（token 按 ER-11 脱敏，不得进验收证据）；漏做这一步会让全部 push/tag/LFS 用例以 401 失败，**不得**判为协议实现回归；⑦ **LFS 卡必须同时设 `MONOENGINE_GIT_SMOKE_PUSH=1 MONOENGINE_GIT_SMOKE_LFS=1`**——LFS 矩阵嵌在 push 分支内，只设 `MONOENGINE_GIT_SMOKE_LFS=1` 时脚本只打印 `SKIP: HTTP LFS push/clone also requires MONOENGINE_GIT_SMOKE_PUSH=1` 且仍以退出码 0 结束，该 skip **不得**作为 LFS 证据（属「Verification 判定口径」禁止的幽灵验收）；LFS 验收必须断言输出出现 `PASS: HTTP LFS push and clone` 且末行 summary 为 `0 failed`，并预装 `git-lfs`（缺失直接判 FAIL） |
   | Cedar 策略与守卫（`src/contract/policy/**`） | 对应单元/集成用例 + 明确断言当前 permit-all 与空 `EntityStore` 前提是否被本卡改变 |
   | 仓库配置与 CI（`Cargo.toml` 非版本行、`rustfmt.toml`、`docker-compose.test.yml`、`scripts/**`、`.github/workflows/**`） | 受影响 CI job 的本地等价命令，按下文「仓库配置与 CI 展开规则」现场提取；不可本地复现的部分归 D 组 |
   | 只改文档 / 索引（无代码、无配置） | 无表面 focused 门，只走 B 组的结构与链接门 |

   **Rust 行的修饰规则（不是独立表面）:** env 约束与线程约束是上面几条 Rust 行的**修饰条件**，不构成独立表面：先按测试归属唯一选中 `-p monoengine --lib` 或 `-p monoengine --test <target>` 中的一行，再把该 target 实际需要的 env 变量与线程约束**并入同一条命令**，例如 `source .env.test && cargo test -p monoengine --test integration_vault -- --test-threads=1`（`source .env.test &&` 前缀不可省略）。当前工作区**没有任何 cargo feature**（唯一 package `monoengine` 的 `Cargo.toml` 无 `[features]` 段，2026-08-27 核对），因此 `--all-features` 不改变本仓编译内容，不要用它冒充一条独立的 focused 门；它只保留在 clippy 全局门里。

   **默认包与默认 target 选择（本仓事实，2026-08-27 核对；直接影响门的覆盖面）:** 自 `plan-20260824` 单体内联后本仓是**单 package** 仓库——根 `Cargo.toml` 只有 `[package] name = "monoengine"`，**没有** `[workspace]` 段，也没有任何子 package（`rg '^\[workspace\]' Cargo.toml` 零命中，全仓只有一个 `Cargo.toml`）。因此 `-p monoengine` 在本地命令里与省略它**等价**；模板和任务卡仍统一写全，只是为了命令可直接复制、且在未来重新拆包时不失效。真正需要显式限定的是 **target**：这一个 package 同时含 lib target `monoengine_core`、两个 bin target（`monoengine`、`migrate_local_to_s3`）和 `tests/` 下的 8 个集成 target，不带 `--lib` / `--test <target>` 的 `cargo test` 会把它们全跑一遍，focused 门必须靠 `--lib` 或 `--test <target>` 收窄（这也是上表每行都带 target 限定的原因）。反过来，`cargo build`、`cargo build --tests`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo +nightly fmt --all --check`、`source .env.test && cargo test --all` 在单包形态下**都覆盖全部 target**（含两个 bin），旧模板记录的「全局 clippy 门漏掉 `bin` 包」缺口已随单体内联消失。唯一仍需消歧的是 `cargo run`：两个 bin target 由 `Cargo.toml` 的 `default-run = "monoengine"` 兜底，要跑另一个必须显式 `--bin migrate_local_to_s3`。

   **不得**为了凑一条 Rust 命令而制造与本卡无关的用例；也不得因为「C 组已有全量测试」就跳过 A 组某个表面的行。

   **仓库配置与 CI 展开规则:** 受影响 job 必须**从本卡实际改动的那个 workflow 文件现场提取**。模板不保存 job→命令的映射快照（必然漂移）。两步：

   ① 列出 job，且必须限定在 `jobs:` 节点内（裸 `rg "^  [a-z...]:"` 会把 `on:` 下的 `push`/`pull_request`、`permissions:` 下的 `contents` 误报为 job）：

   ```bash
   awk '/^jobs:/{in_jobs=1; next} in_jobs && /^[^[:space:]]/{exit} in_jobs && /^  [A-Za-z0-9_-]+:/{print FNR ":" $0}' .github/workflows/<file>.yml
   ```

   ② 读取受影响 job 的**完整定义**——`permissions`、`strategy` / `matrix`、job 与 step 级 `env`、`if`、`working-directory`、`run`、`uses`、`with`——再据此写判据。只看 `run:` 不够：`claude-review.yml` 当前**没有任何 `run:` 步骤**，其行为完全由 action 定义。分流：
   - **可本地复现**的步骤 → 抄成命令进 A 组，保留其 `env`、`working-directory`、线程与 URL 约束。**依赖 GitHub checkout 的步骤不得照抄**：`actions/checkout` 对应的本地事实是「当前工作树即 checkout」。**（2026-08-27 订正：本条旧文写「私有 `orbit` 仓的 checkout 对应 sibling `../orbit` 已就位」，该映射已随 `plan-20260824` 单体内联失效——workflow 里已无 orbit checkout 步骤，`ORBIT_CHECKOUT_TOKEN` 只作为 `WEBSITE_CHECKOUT_TOKEN` 的 fallback secret 名残留。）**当前 workflow 里唯一的私有仓 checkout 是 `config-validation.yml` 的 `gitmono-dev/monoui`（固定 ref、`path: monoui`），其本地等价是 sibling `../monoui` 已就位；token 存在性校验与 `::error` 分支不可本地复现，归 D 组。`${RUNNER_TEMP}` 换成本地临时目录，`>> "$GITHUB_ENV"` 换成同一 shell 会话内的 `export`。
   - **不可本地复现**的语义（`::add-mask::`、secret 存在性校验、私有仓 token checkout、`anthropics/claude-code-action`）→ 不得凭空造命令，归入下文 **D 组远端后置门**。

   参考：2026-07-29 快照下 `.github/workflows/` 只有三个文件、每个文件恰好一个 job——`claude-review.yml` → `claude-review-with-tracking`（仅 `issue_comment` / `pull_request_review*` / `issues` 事件触发，**不由 push 触发**，且 100% action-only）、`config-validation.yml` → `validate-config`、`git-protocol-smoke.yml` → `git-protocol-smoke`（后两者均为 `pull_request` + `push: branches:[main]`，且都带 `paths:` 过滤器）。仅核对日快照，判定一律以现场读取的 workflow 文件为准。

   **B 类型门**（按 `Task type` 取一行）：

   | Task type | 类型门 |
   |---|---|
   | `implementation` / `migration` / `removal` | 无额外类型门（由 A 组 + C 组构成完整验收；`family child` 自跑 A 组 + fmt/clippy，C 覆盖继承自家族唯一发布点） |
   | `docs` / `audit` / `handoff` | 结构与链接门：本卡产物文件存在且章节完整、内部链接与 `file:line` 锚点可解析、新引入的 `docs/*.md` 路径全部真实存在（本仓已有多处悬空文档引用，不得新增）、`git status --short --branch` 无越界改动。这三类**必须保持 no-code / no-config**：一旦发现需要改动代码或配置，不得「就地升级门」，必须先按 ER-03 把卡重分类为 `implementation` / `migration` / `removal`，同步 `Release boundary`、`Version increment`、`Release write set`，重跑粒度门后再执行 |
   | `spike` | 产物门：结论文档或 ADR 已落盘、go/no-go 已判定、承接卡已登记；**allowlist diff 门**——`git status --short --branch` 的全部变更必须落在本卡 `Deliverables` 声明的产物内，且生产表面零改动（至少覆盖 `src/**`、`tests/**`、`config/**`、`scripts/**`、`Cargo.toml`、`Cargo.lock`、`rustfmt.toml`、`docker-compose.test.yml`、`.github/workflows/**`），用「Verification 判定口径」的退出码模板逐条守卫 |
   | `release` | 聚合守卫（本组引入的全部新守卫用例）+ release note / 兼容证据 |

   **C 发布收口门（由会推送的卡执行，顺序强制）:** ① 版本面 parity 预检（ER-08）→ ② 按 `Version increment` bump 版本面（当前为**一处**：`Cargo.toml` 的 `version`，处数以 ER-08 的开工日核对为准）+ 让工具链刷新 `Cargo.lock` 的对应条目（不手改）→ ③ 在**已 bump 的状态**上跑 `AGENTS.md` 三门（fmt、clippy、`source .env.test && cargo test --all`）→ ④ `cargo build` 与 `cargo build --tests` 均 0 错误 0 警告；需要可执行产物时另跑 `cargo build --release -p monoengine` → ⑤ `libra add <相关路径>` + `libra commit -m` → ⑥ 推送并确认 branch ref：`libra push origin main` 成功且远端 ref 已更新 → ⑦ 对每次实际版本 bump，在该提交上创建同名标注 tag：`libra tag -m "v<version>: <summary>" v<version>`，并以 `libra push origin refs/tags/v<version>` 推送 → ⑧ 分析该版本最终 diff，人工撰写标准 release note 后执行 `gh release create v<version> -R gitmono-dev/monoengine --title "v<version>" --notes-file <release-notes-file>`。release note 必须包含 Highlights、用户可见变更、兼容性 / 配置 / 迁移说明（无则写 `N/A`）、验证结果和已知限制；禁止使用 `--generate-notes` 或其它自动生成内容。`Version increment=N/A` 的卡不创建 tag 或 release。

   三门必须覆盖 bump 后的最终状态——bump 与 `Cargo.lock` 刷新本身可能引入格式、lint 或编译回归，`cargo build` 不能替代 clippy 与全量测试。

   **C / D 边界（唯一口径）:** C 组**截止到已验证的 branch 推送**；推送之后由远端流水线产生的一切证据全部归 **D 组**。任务卡必须为每个 D 组项登记：workflow 文件、job 名、**触发事件与 ref**、**paths 过滤器是否命中本卡改动**、以及判据。

   **D 远端后置门（post-push，仅在有不可本地复现的 CI 语义时适用）:**
   - 本仓 D 组的实际内容是：`config-validation.yml` 与 `git-protocol-smoke.yml` 在 `push: branches:[main]` 上的远端结论——**当且仅当本卡改动命中各自的 `paths:` 过滤器**；以及这两个 workflow 中本地无法复现的部分（私有 `gitmono-dev/orbit` token checkout、`::add-mask::` 日志脱敏、secret 存在性校验）。
   - `claude-review.yml` **不由 push 触发**（只响应 issue/PR 评论与 review 事件），因此直推 `main` 的卡不得把它登记为 D 组项；只有走 PR 流程的卡才可能命中。
   - 若本卡改动不命中任何 `paths:` 过滤器，D 组写 `N/A` 并说明「改动路径不在两个 workflow 的 paths 列表内」，不得虚构一个远端门。
   - D 组**不属于**本地验收，不阻塞 `locally-accepted`，也不改变 ER-05「先本地验收再 review」与 C 组「review 通过后才提交推送」的顺序。
   - 只有当适用的 D 组门也全绿，该卡的 `Acceptance` 才能到 `complete`；此前停在中间态 `remote-pending`。
   - D 组失败一律**前滚修复**（新提交 / 新版本），不得回退已推送提交；修复卡按 ER-10 的越界规则处理。

   「完成判据」的计划级门是最后一次总检查，不替代每张会推送的卡各自跑过的发布收口门。
5. **ER-05 代码 review 闭环:** 实现和本地验收完成后进行代码 review；review 问题修复后重跑相关验收，直到 review 明确给出 `PASS`。P0/P1 必须关闭，不得以「residual risk 已接受」替代 `PASS`（仅 P2 可由具名责任人书面接受）。
6. **ER-06 文档与兼容同步:** 涉及公开行为的任务必须同步用户文档、开发文档、`config/config.toml` 示例、错误契约、运行时 OpenAPI 证据和测试矩阵。
7. **ER-07 提交工作流与提交签名:** 本仓库使用 Git 工作流：`git status`、`git add <相关路径>`、`git commit -m "<scope>: <summary>"`、`git push origin main`。（**2026-08-27 变更**：本条原名「Libra-native 工作流与提交签名」，`.libra` 删除后改为 Git；历史计划中的 `libra *` 命令是当时的事实记录，不回改。）
   - 签名策略：`commit.gpgSign=true` 时 `git commit` **自动签名**，无需显式 `-S`；显式 `-S`/`--gpg-sign` 与 `--no-gpg-sign` 可逐次覆盖。优先级为命令行 `--no-gpg-sign` / `-S`（最高）> `commit.gpgSign` > 不签名。`-s` 是 `Signed-off-by`，与 GPG 签名是两件事，按下条的仓库惯例决定是否使用。
   - 预检读 `git config --get commit.gpgSign` 与 `git config --get user.signingkey`；二者任一缺失或 `commit.gpgSign=false` 时不得当作「已启用签名」。
   - 每次提交后强制校验：`git cat-file -p HEAD | rg -q '^gpgsig'`。校验失败不得推送。
   - **本仓已知现状（2026-08-27 核对）:** `commit.gpgSign=true`、`user.signingkey=7B2F49AE3A9E8BDC`，实测最近提交**均带 `gpgsig` 头**——即旧版记录的「配置为签名但实际不签名」缺口在 Git 下已消失，`EX-*` 豁免路径当前无需启用。若某次校验仍失败（签名密钥不可用、agent 未解锁等），必须查明原因并记录，不得静默跳过；确需以 sign-off-only 方式提交时，仍按「字段全局默认与例外」登记 `豁免项 = ER-07 签名要求` 的 `EX-*`（含 Approver、Review round、证据、有效期），「事实基线」最多链接该 `EX-*`，不构成独立授权路径。
   - **与 `README.md` / `AGENTS.md` 的已知漂移及优先级（必须按此执行）:** `README.md` 的 Contributing 一节写的是「Sign your commits (`git commit -s -S …`)」——该命令**现已可执行**（旧版记录的「双重不可执行」随 `.libra` 删除而失效），但与本仓实际惯例仍有一处差异：近期提交**不带 `Signed-off-by`**，签名由 `commit.gpgSign` 自动完成而非显式 `-S`。`AGENTS.md` 则**完全没有**提交/分支/签名指引，同时含若干过期事实（`src/common/config/loader.rs` 的真实路径是 `src/config/loader.rs`；把 `cargo test` 写成全量测试，与其自身收口门的 `source .env.test && cargo test --all` 不一致；子命令 executor 签名**待复核**）。**（2026-08-27 订正：本条旧文还把「单一 binary crate」「`src/main.rs`」列为过期事实——那是双 package 拆分期的判断；`plan-20260824` 单体内联后本仓确实是单 package、入口确实是 `src/main.rs`，AGENTS.md 这两处已与事实一致，不再计为漂移。）**计划执行时**以 ER-07 + GC-12 为准**：`git add <相关路径>` → `git commit -m` →（按上三条做签名预检与提交后 `gpgsig` 校验）。这些漂移应作为独立的文档修复项承接，不得在计划执行中两套并行。
   - 提交信息沿用本仓已观察到的两种既有风格：发布类改动用 `v<version>: <summary>`；文档/测试/CI 类改动用 `<type>(<scope>): <summary>`。
8. **ER-08 版本与发布:** 版本权威源是根 `Cargo.toml` 的 `version`。**版本面自 plan-20260824 单体内联后只有一处**（2026-08-27 核对）：唯一 package 是 `monoengine`（`Cargo.toml` `[package] name`，lib target 名 `monoengine_core`，另有两个 `[[bin]]` target `monoengine` / `migrate_local_to_s3`）；`bin/Cargo.toml` **已不存在**，`crates/orbit*` workspace 成员与 sibling `../orbit` 也已随内联移除。因此发布前的「版本面 parity 预检」在当前拓扑下退化为空操作，但**开工时仍须重新核对版本面文件数量**（`rg -n '^version' Cargo.toml` 与 `rg -l '^\[package\]' --glob '**/Cargo.toml'`）——若未来重新拆包，parity 预检与「不一致时先建立修复卡对齐、禁止直接 bump」的规则立即恢复适用。按任务卡的 `Version increment` 递增该处 version，让工具链刷新 `Cargo.lock` 的对应条目，其余步骤与顺序按 ER-04 的「发布收口门」执行（bump 后必须重跑三门）。
   - **包名口径**：`cargo` 命令一律用 `-p monoengine`；模板与存量计划里出现的 `-p monoengine-core` 是单体内联前的旧包名，已失效（同一裁定见 `plan-20260827.md` 的「包名口径」与其 R5 评审记录）。
   - `Version increment` 取值：`patch`（默认）| `minor` | `major` | `N/A`。
   - 删除公开 surface、破坏兼容的 schema/协议变更必须用 `minor` 或 `major`，且递增级别由 ADR + 兼容窗口证据决定，不得用 patch 夹带；家族卡（G-08）的递增级别写在唯一发布点卡上，子卡为 `N/A`。
   - `docs` / `audit` / `spike` / `handoff` 卡为 `N/A`，但必须说明产物随哪次提交进入仓库。
   - 每次实际版本 bump 必须创建并推送 `v<version>` 标注 tag，并以人工编写的标准 release note 创建同名 GitHub Release。release note 必须基于最终 diff，包含 Highlights、用户可见变更、兼容性 / 配置 / 迁移说明（无则写 `N/A`）、验证结果和已知限制；禁止 `--generate-notes`。`Version increment=N/A` 的卡不创建 tag 或 release。
9. **ER-09 push 失败策略:** 非 fast-forward 需要 pull/merge 后重新验收再推；认证、权限、网络或服务端失败不 blind retry，记录原因，待下一次修复/发布窗口处理。
10. **ER-10 内部服务错误（有界重试）:** Redis、Postgres、对象存储、SMTP、AI provider 等错误不得直接把任务宣告完成。先分类：确定性错误（4xx 参数/权限、schema 不符、编译或配置缺陷）不重试，按范围决定归属——只有当修复落在**本卡行为轴内**，且先更新本卡 `Acceptance criteria` / `Verification` / `Implementation write set` / `Granularity` 后**重跑 ER-03 的全部 `G-*` 仍然通过**（含 G-10 写集不与在跑卡新冲突）时，才作为本卡修复项就地修；任一条不满足就新建修复卡 `FIX-*`、加一条 `FIX-* -> 当前卡` 的依赖边，并把当前卡置为 `blocked`，不得为了「顺手修完」突破粒度；暂时性错误（超时、5xx、限流、网络中断）按指数退避重试，并写明**最大尝试次数与总时间预算**（计划未另行规定时默认 ≤ 5 次、总计 ≤ 30 分钟）。超预算后把任务置为 `blocked` 并记录 sanitized 证据与升级对象，不得静默空转。发布类动作（push）不自动重试，按 ER-09 处理。
11. **ER-11 证据卫生:** 验收证据不得保存 secret、API key、token、PII、未脱敏 transcript、绝对私有路径或原始 tool payload。需要留存时只写 sanitized summary。测试栈的口令（如 `monoengine_test_password`、`smtp-test-password`、RustFS 测试凭据 `rustfs` / `rustfs_secret`）虽为公开测试值，记录时同样按脱敏处理。
12. **ER-12 并发边界与串行发布:** 并发只适用于**实现与 review 阶段**：只有 `Implementation write set` 不相交（G-10）的卡可以并发推进。**发布动作一律串行，且由单一发布者执行**：
    - 计划必须在「发布分组与并发窗口」声明发布者（哪个 Agent/人负责 C 组的 bump、构建、提交、推送，以及推送后跟踪 D 组远端证据）。同一时刻只允许一个卡处于「已 bump 未完成推送」状态。
    - 进入发布前重新读取 `Cargo.toml` 权威版本（ER-08 的 parity 预检），按顺序做完整套发布动作后才轮到下一张卡。
    - **禁止多 Agent 并发发布。** 本仓库当前**没有**仓库级发布锁：`git push origin main` 推送的是本地 `main` 的整个 ref tip，无法只发布一条协调记录，也无法在 push 之外提供 CAS 仲裁；靠纯文档约定实现的 lease 无法验证，属于未经实现验证的协议。若某计划确实需要并发发布，必须先用独立 ADR + 独立计划落地一个仓库级发布锁（含原子认领、fence 校验、超时回收、崩溃恢复与测试），并在本计划以 `DEFER-*` 登记；在该机制落地并通过验收之前，一律按本条串行执行。
    - 并发实现期间仍受 I–R 约束（G-10）：发布者持有发布窗口时，其它卡不得修改 `Release write set` 内的文件。

## 实施顺序

依赖边格式：`A -> B` 表示 A 必须先于 B。依赖图必须无环，且每条边都指向具体任务卡而不是整个 Phase（G-06）；任务卡拆分后本节必须同步更新。

- `<TASK-01> -> <TASK-02>`
- `<TASK-02> -> <TASK-03>`

### 依赖登记表

本计划外的一切依赖关系（其它日期计划、外部服务、人工审批、上游 revision），以及本计划向外移交的范围，都必须在此登记后才能被任务卡引用（G-06）。计划内任务之间的依赖直接写任务 ID，不进本表。

`direction` 区分方向：`incoming` = 本计划等待外部产物；`outgoing` = 本计划把范围移交给别的计划（此时 Owner 是接收方，「超时与失败策略」写接收方未接手时的回落处理）。

| ID | direction | 类型 | 对象 | Owner | 产物与可用性判据 | 证据 | 超时与失败策略 |
|---|---|---|---|---|---|---|---|
| DEP-01 | `<incoming / outgoing>` | `<跨计划 / 外部服务 / 审批 / 上游 revision>` | `<plan-YYYYMMDD#TASK-ID、plan-long#PT-NN 或外部对象>` | `<负责人/系统/接收方计划>` | `<交付什么、如何判定可用>` | `<file:line / commit / URL + 核对日期>` | `<等待上限、超时后降级或回落路径>` |

### 发布分组与并发窗口

默认每张卡独立发布（G-07）。只有需要合并发布或需要显式并发/串行窗口时才登记本表；登记项必须在任务卡 `Release boundary` 中被引用。

| ID | 成员 | 唯一发布点 | 窗口规则 | 失败回滚顺序 | 理由 |
|---|---|---|---|---|---|
| REL-01 | `<TASK-ID 列表>` | `<TASK-ID>` | `<例如：不推送窗口——子卡只本地提交，不 bump、不推送；窗口期禁止插入其它切片>` | `<按依赖逆序 revert 本地提交并重跑 ER-04>` | `<为何无法拆成独立发布切片>` |

**并发声明:** `<实现阶段可并发的卡组（实现写集互不相交）/ 全串行>`（G-10）

**发布者:** `<负责 C 组 bump/构建/提交/推送，并跟踪 D 组远端证据的唯一 Agent/人>`（ER-12：发布一律串行，禁止多 Agent 并发发布）

**发布窗口顺序:** `<按依赖与 REL-* 分组列出发布顺序；同一时刻只允许一张卡处于「已 bump 未完成推送」状态>`

并发执行时另需满足：实现写集不相交（G-10）。

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

任务 ID 使用稳定前缀，例如 `IT-01`、`A0-01`、`DR-01`、`P0-03`。编号被引用后不重排；拆分出的新卡在所属 Phase 末尾追加新编号，因此**编号顺序 ≠ 执行顺序**，执行以「实施顺序」的依赖边和各卡 `Dependencies` 为准。废弃的编号保留并标记替代关系。

### 任务卡粒度规则（强制）

粒度是任务卡质量的第一判据：卡过大则无法 review、无法回滚、无法交给单个 Agent 完成；卡过碎则实现、测试与文档脱节，且每片都要付一次发布成本。新增或修改任务卡时必须逐条满足下列 `G-*` 规则。任一条不满足即为「粒度不合格」，必须在开工前拆分或合并，并同步实施顺序、依赖登记表、发布分组、追溯表、测试矩阵、里程碑、风险表的任务归属和修订历史。

- **G-01 单一行为轴与恢复模式（上限）:** 一张卡只承担一个**可独立恢复**的行为轴——该卡失败或需要撤回时，存在**单一已声明的恢复动作**，执行后系统停在一个自洽状态，不留半吊子中间态。这里的「恢复」不等于「一次 revert」：不可逆变更同样合格，只要恢复路径是单一且已写明的。禁止把「schema 变更 + 存储层接入 + API 暴露」「删 A + 删 B + 删 C」「新增能力 + 顺带重构既有实现」压进同一张卡。快速判据：`Description` 中出现两个以上并列的「并且 / 同时 / 顺带 / 以及」，先按「推荐拆分维度」拆。恢复形式必须在 `Rollback mode` 字段声明为四种模式之一：
  - `revert`：纯本地代码/文档变更，一次 revert 即完整撤销（默认）。
  - `forward-only`：已产生不可逆数据或迁移（Postgres schema、对象存储、vault secret），只能前滚修复；必须写出数据不变量、恢复验证命令和用户影响，**不得**为了凑「可 revert」而设计不安全的 down migration。注意本仓有相当一部分既有迁移的 `down` 是空实现（开工时以实际迁移文件为准），且运行期没有任何 `migrate down` 入口——`src/jupiter/migration/runner.rs` 只暴露 `Migrator::up` 与破坏性的 `Migrator::refresh`。因此涉及既有迁移的卡默认按 `forward-only` 处理，除非本卡自己实现并验证了真实的 `down`。
  - `compensating`：对外部服务已有副作用（远端写入、邮件发送、外部对象删除），撤销靠补偿动作；必须写出补偿命令与幂等键。
  - `immutable-release`：已推送的提交不可撤回，只能前滚新版本；必须写出降级指引与兼容窗口。
- **G-02 完整可交付（下限）:** 一张卡必须是一个自洽的可验收增量。同一行为轴的实现、测试、文档与索引同步是**同一张卡**的验收内容，禁止拆成「实现卡 / 补测试卡 / 补文档卡」。只有当被拆出的部分本身就是独立可恢复的行为轴（G-01 意义上：有单一已声明的恢复动作——独立迁移、独立 deprecation 收口、独立性能门、独立 API 切面、跨计划移交）时才允许单独成卡。
- **G-03 条目上限与计数口径:** 上限按 `Task type` 取值，计数按**独立判据**而非行数。ER-04 的强制门（fmt / clippy / 全量测试 / 表面门）**不计入**本条计数，`Verification` 只登记本卡特有的判据：

  | Task type | AC 上限 | Verification 上限 |
  |---|---|---|
  | `implementation` / `migration` / `removal` | 8 | 8 |
  | `spike` | 8 | 8 |
  | `docs` / `audit` / `handoff` | 20 | 20 |
  | `release` | 12 | 12（只计聚合守卫、release note、兼容证据等本卡特有项） |

  计数细则：
  - AC 按「独立 pass/fail 谓词」计。一条 checklist 内用「且 / 并且 / 以及 / 同时」连接的多个可分别失败的断言按多条计；嵌套子列表逐项计；表格行逐行计。
  - Verification 按「独立验证门」计。判据是「是否构成一次独立的通过/失败判定」：环境准备前缀（`source .env.test`、`MEGA_*=…` 赋值、`docker compose … up -d --wait`、`export`）与其后的命令合计为**一门**；一条命令中的多个 `--test` target 分别计；`&&` 串联两个都会独立判定的验收命令按两门计；手工证据按项计。
  - 超限视为多轴信号，必须拆卡，**不得**通过合并长句、塞进表格或改写成「等等」来规避。
  - 文档 / 审计 / 索引-only 卡的条目是清单项、不构成独立行为轴，故适用 20 条上限；这类卡仍受 G-01 约束，且必须在任务卡 `Deliverables` 字段登记产物范围（具体文件清单）——这是常规登记，不是例外，无需进 waiver 表。需要突破本表上限时，只能在 waiver 白名单登记 `EX-*`（具名审批），不得私自改写分母。
- **G-04 规模上限（可计数）:** `Estimated scope` 的开工态只允许 `S` 或 `M`。`L`/`XL` 只能作为「必须再拆」的中间标注，计划成稿后不得存在 L/XL 卡。计数**只统计行为实现落点与生产文件**，不统计「随附同步集」：
  - **计入**：承载本卡行为变更的生产代码落点与文件（`src/**`、`config/**`、`scripts/**` 等）。
  - **不计入（随附同步集）**：本卡自己的测试文件、按 GC-05/ER-06 强制同步的文档集（`docs/**`、`README.md`、`config/config.toml` 注释）、以及 ER-08 的版本面（当前一处）。这些是每张卡的固定成本，不构成粒度信号；但仍要在写集字段中如实列出（文档/测试进 `Implementation write set`，版本面进 `Release write set`）。
  - **仓库根文件**（`Cargo.toml`、`rustfmt.toml`、`docker-compose.test.yml` 等）按「单个文件」计，不各占一个落点；若某张卡的行为变更**就发生在**根文件本身（例如改 `docker-compose.test.yml` 的服务拓扑），则该文件计为一个落点。
  - `S`：≤ 2 个行为落点、≤ 3 个生产文件，无 schema / 协议 / 公开接口变更。
  - `M`：≤ 4 个行为落点、≤ 12 个生产文件，最多一处公开行为或接口变化，仍是单一行为轴。
  - 超出 `M` 的计数即为 L：默认必须拆分。确实不可拆的机械变更（全仓重命名、批量删除、格式化）可在「字段全局默认与例外」的 waiver 白名单中登记 `EX-*`（需具名审批人与 review 轮次），写明为何不可拆、如何 review、如何恢复；此时该卡 `Estimated scope` 写 `L-exception:EX-<n>`，这是全文唯一允许出现 `L` 字样的形式，`XL` 永不允许。
  - 把 `src/`、`tests/`、`docs/` 或仓库根算作「一个落点」是规避行为，按「粒度反模式速查」的「落点注水」处理。
- **G-05 Agent 可独立执行:** 一张卡必须能在不阅读其它卡正文的前提下被执行：`Current evidence` 给出可核对的 `file:line` 锚点，`Acceptance criteria` 自洽可判定，`Verification` 是可直接复制执行的确切命令，`Dependencies` 只引用「依赖登记表」中的 `DEP-*` / 任务 ID。禁止「见上文」「同上一卡」式跨卡隐式约定；确属跨卡共享的约定要提升为全局工程约束或 ADR。
- **G-06 依赖闭合且无环:** 依赖必须有向无环。本计划内依赖直接引用任务 ID；跨计划与外部前置必须先在「依赖登记表」登记为 `DEP-*` 再引用，不得在卡内自由描述。互相等待、循环依赖、以及「等某个 Phase 整体完成」都是拆分错误——把依赖收敛到具体前置卡。「实施顺序」的依赖边与各卡 `Dependencies` 必须一致；不一致时以「实施顺序」为准并当场修正卡片。
- **G-07 发布切片对齐（按任务类型）:** 默认「一张卡 = 一个发布切片」（独立 review + ER-04 门 + 版本 + 提交 + 推送）。适用范围按 `Task type`（G-11）区分：`implementation` / `migration` / `removal` 必须走完整发布切片；`docs` / `audit` / `spike` / `handoff` 卡不 bump 版本，`Release boundary` 写 `no-release` 并说明其产物随哪次提交进入仓库；`release` 卡本身就是发布点（家族卡的唯一发布点必须是 `release` 卡，见 G-08）。任何「多卡合并发布」都是例外，必须在「发布分组与并发窗口」登记 `REL-*`：成员、唯一发布点、窗口期禁止插入的内容、失败时的逆序回滚顺序。例外必须先修订计划并通过 review 才可开工，**不得**在开工时凭笔记临时合并。
- **G-08 家族卡（不可分割变更的唯一出路）:** 当一次公开 surface 删除、或 schema 与 reader 必须同时上线这类变更确实无法切成可独立发布的切片时，用「家族卡」表达：拆成多张各自 review、各自通过全部适用 ER-04 门、各自本地提交的子卡，共用一个唯一发布点卡；**该发布点卡的 `Task type` 必须是 `release`**（不引入新行为，只做版本、构建、聚合守卫与发布证据），以保证它在 ER-04 的 B 组中唯一命中 `release` 行。家族内子卡仍受除 G-07 外的全部 `G-*` 约束；子卡 `Release boundary` 写 `family child`，发布点卡写 `family release point`，家族边界与「不推送窗口」写进 `REL-*` 登记。
- **G-09 拆分协议:** 拆分已被引用的卡时，原编号保留给主轴，新子卡在所属 Phase 末尾追加新编号，不重排既有编号。原卡必须写明「拆出 `<ID>`、`<ID>`」，新卡写明「自 `<ID>` 拆出」，并同步实施顺序、依赖登记表、「发布分组与并发窗口」、追溯表、测试矩阵、里程碑、风险表，以及「修订历史」中的一行（日期、原因、原卡、新卡、受影响引用）。
- **G-10 写集与并发:** 写集分三类，每张卡必须声明前两类（第三类由 ER-12 统一定义，卡内不重复）：
  - **`Implementation write set`（I）**：承载本卡行为的代码、测试、文档文件。
  - **`Release write set`（R）**：ER-08 的版本面（当前**一处**：`Cargo.toml` 的 `version`，2026-08-27 核对）+ `Cargo.lock`。对所有发布卡相同；`family child` 与 `no-release` 卡写 `N/A`（它们不 bump、不推送）。
  - **协调写集（C）**：计划级的发布顺序与窗口记录（「发布分组与并发窗口」的发布者、发布顺序、`REL-*` 登记）。由 ER-12 的单一发布者串行维护，**不计入**任何卡的 I 或 R，也不参与并发判定。

  冲突规则：
  - **I–I 相交** → **禁止并发，无豁免通道**（G-10 不在 waiver 白名单内）：只有两个合法出路——补一条顺序依赖边，或把相交部分合并到唯一集成卡。
  - **I–R 相交**（某卡把 `Cargo.toml`、`Cargo.lock` 等当行为落点，而它同时属于别的卡的 R）→ 在**已声明的串行发布窗口**内（ER-12），该窗口对 R 内文件是写锁：其它卡不得在此期间修改这些文件，必须等窗口结束或补顺序边。
  - **R–R 相交** → 由 ER-12 的串行发布窗口顺序化，不构成并发禁止条件。
  - `Files likely touched` 是估计值，并发判定以 `Implementation write set` 为准。
- **G-11 任务类型:** 每张卡必须声明 `Task type`，不同类型适用不同粒度口径：
  - `implementation`：默认类型，全部 `G-*` 条款全量适用。
  - `migration`：数据/schema 迁移，`Rollback mode` 通常为 `forward-only`，必须有 up/down 或前滚验证与故障注入用例，并在 `src/jupiter/migration/mod.rs` 的 `migrations()` 列表登记顺序。
  - `removal`：公开 surface 删除，通常进入家族卡（G-08），必须先有 deprecation 窗口证据。
  - `spike`：探索/验证，**不得**改动生产代码。必须写出待回答的问题、时间箱、产物（结论 + ADR 或缺口登记）、go/no-go 退出标准与后续承接卡；不适用 G-04 的文件计数，`Estimated scope` 按时间箱判定：`S` ≤ 0.5 人日、`M` ≤ 2 人日，超出即拆成多个问题或直接转 ADR / `implementation` 卡。
  - `audit` / `docs`：只读核对或文档收敛，按 G-03 的文档-only 口径执行。规模上限按**产物文件数或人日**判定（不适用 G-04 的生产文件计数）：`S` ≤ 5 个产物文件或 ≤ 0.5 人日；`M` ≤ 15 个产物文件或 ≤ 2 人日；超出即拆卡。随代码卡强制同步的文档仍按 G-04 的随附同步集处理，不计入这里。
  - `release`：发布点卡，不引入新行为，只做版本、构建、聚合守卫与发布证据。
  - `handoff`：跨计划移交，默认 `no-release`。**移入**（本计划承接他人）在「依赖登记表」登记 `direction: incoming`；**移出**（本计划把范围交给别的计划或 `plan-long.md` 的 PT 项）登记 `direction: outgoing`，并写明接收方、移交日期、本计划不再重做的部分，以及接收方未接手时的回落处理。

#### 推荐拆分维度

超限卡按下列维度之一切开；切完每片仍须独立满足 G-01（单一行为轴 + 已声明的恢复模式）。

| 维度 | 切法 | 典型结果 |
|---|---|---|
| 数据 / 状态轴 | entity + migration → storage 写入与幂等 → 读取投影与恢复 | 3 张卡 |
| 协议轴 | 协议版本与协商 → 容量 / 背压与性能门 → 消费端接入 | 3 张卡 |
| 表面轴 | 后端 service/storage → 机器接口（HTTP JSON / OpenAPI / 错误映射） → CLI 或客户端接入 | 2–3 张卡 |
| 生命周期轴 | 新实现上线 → 默认切换 → deprecation shim → 物理删除 | 按发布窗口分卡 |
| 安全轴 | 身份与请求边界（Cedar / token） → 路径与对象归属 → 敏感信息 redaction | 每轴一卡 |
| 清理轴 | 公开 surface 删除（家族卡） → 内部模块退场 → 依赖摘除 | 家族卡 + 普通卡 |

#### 粒度反模式速查

| 反模式 | 症状 | 处理 |
|---|---|---|
| 巨型卡 | `Estimated scope` = L；AC > 8；Description 含多个并列目标 | 按「推荐拆分维度」拆分（G-01/G-03/G-04） |
| 碎片卡 | 「补测试」「补文档」「改个字段名」单独成卡 | 合并回所属行为轴（G-02） |
| 多轴伪装 | 把多条 AC 合成一条长句、塞进表格或写「等等」以压到 8 条以内 | 按独立谓词还原计数后重新判定（G-03） |
| 落点注水 | 把 `src/` 或仓库根算作「一个落点」以保住 S/M | 按目录级落点重新计数（G-04） |
| 隐式依赖 | Description 写「按 X 卡的约定」而 X 卡未交付该约定 | 写进本卡，或提升为全局约束 / ADR（G-05） |
| 幽灵验收 | `Verification` 只写 `cargo test --all`，或零命中守卫不区分 `rg` 退出码 `1` 与 `>1` | 指定 package/target 与 test fn；按「Verification 判定口径」的退出码模板重写（G-05） |
| 悬空依赖 | `Dependencies` 写「Phase N 完成」或自由描述外部前置 | 收敛到具体前置卡 ID / `DEP-*`（G-06） |
| 假回滚 | 已推送或已迁移数据的卡仍写「一次 revert 撤销」 | 按实际选 `forward-only` / `compensating` / `immutable-release`（G-01） |
| 并发冲撞 | 两张无依赖的卡实现写集相交 | 只有两条出路：补顺序边，或合并到唯一集成卡（G-10 不可豁免）。版本面争用不算并发冲突，由 ER-12 的串行发布窗口处理 |
| 顺手合并 | 多张卡凭开工笔记合成一次发布 | 登记为 `REL-*` 家族卡，或拆回独立发布切片（G-07/G-08） |

#### 字段全局默认与例外

计划在本节声明字段的全局默认值后，任务卡中**取默认值的字段可以整行省略**，或写 `Inherited`；只有偏离默认的字段才在卡内展开并在下表登记。`Task type`、`Lifecycle / Acceptance`、`Rollback mode`、`Implementation write set`、`Version increment`、`C/D coverage from`、`Granularity` 摘要行是每卡必填，不可省略；`Release write set` 可写 `Inherited` 或 `N/A`；`Deliverables` 对 `docs` / `audit` / `spike` / `handoff` 卡必填。

- **Release boundary 默认:** `<每张卡独立发布切片 / 其它>`
- **Task type 默认:** `<implementation / 其它>`
- **Rollback mode 默认:** `<revert / 其它>`
- **Migration and rollback 默认:** `<N/A：无 schema 迁移 / 其它>`
- **Security and privacy 默认:** `<继承 GC-07、GC-11 / 其它>`
- **Performance budget 默认:** `<继承 GC-10 / 其它>`
- **Docs and compatibility impact 默认:** `<按 GC-05 同步相关 docs/ 文档与 config 示例 / 其它>`

**默认覆盖**（不是例外，只是取了非默认值，无需审批）：

| 任务 | 偏离的字段 | 取值与理由 |
|---|---|---|
| `<ID>` | `<Rollback mode>` | `<forward-only：既有迁移 down 为空实现，只能前滚 + 校验>` |
| `<ID>` | `<Docs and compatibility impact>` | `<仅开发文档，无用户可见命令或配置变化>` |

**规则 waiver（`EX-*`，需具名审批）**：可豁免的规则是**白名单**，只有下表三项；`G-01`、`G-02`、`G-05`、`G-06`、`G-07`、`G-08`、`G-09`、`G-10`、`G-11` **永不可豁免**（它们是可 review、可恢复、可并发的前提）。

| 可豁免项 | 允许的理由范围 |
|---|---|
| G-03 条目上限 | 清单型产物（文档 / 审计 / 索引）确实需要超过本类上限，且已写明产物文件清单 |
| G-04 规模上限（`L-exception`） | 不可拆的机械变更：全仓重命名、批量删除、格式化 |
| ER-07 签名要求 | 仓库策略层面的具名豁免（sign-off-only） |

| 例外 ID | 任务（或 `ALL/<作用域>`） | 豁免项 | 理由与补偿措施 | Approver | Review round | 证据 | 有效期 |
|---|---|---|---|---|---|---|---|
| EX-01 | `<ID>` | `<G-03 条目上限>` | `<文档-only 卡，产物范围 = docs/refactoring/<topic>.md；补偿 = 逐文件 checklist>` | `<具名审批人>` | `<R-n>` | `<file:line / review 结论>` | `<本计划内 / 至 YYYY-MM-DD>` |
| EX-02 | `<ID>` | `<G-04 规模上限>` | `<全仓重命名不可拆；review = 逐目录 diff 抽检 + 守卫用例；恢复 = revert 单提交>` | `<具名审批人>` | `<R-n>` | `<命令与守卫用例>` | `<本计划内>` |

#### 任务卡粒度审计表

计划成稿与每次规范性修订后填一次，逐卡汇总各卡 `Granularity` 行，便于机械核对与脚本校验。判定规则：任一列不达标即不得开工；`AC` / `VER` / `scope` 列超限时必须带 `@EX-ID` 或 `L-exception:EX-n`，且该 `EX-*` 必须同时满足：存在于 waiver 表、`任务` 列等于引用它的卡（或显式写 `ALL/<作用域>` 的计划级豁免）、`豁免项` 等于被超限的那条规则、理由落在白名单、且仍在有效期内。任一条不满足仍判为不达标。

| 任务 | type | axis | recovery | complete | self-contained | AC | VER | landing / prod-files | scope | deps | writeset | release | split-from | exception |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `<ID>` | `<Task type>` | `<行为轴>` | `<恢复动作>` | `<yes>` | `<yes>` | `<n/上限[@EX-ID]>` | `<n/上限[@EX-ID]>` | `<n>/<n>` | `<S/M/L-exception:EX-n>` | `<TASK-ID/DEP-ID/none>` | `<no-overlap/序列化于 ID>` | `<independent/REL-n child/REL-n point/no-release>` | `<ID/N/A>` | `<EX-ID/N/A>` |

#### Verification 判定口径

- **零命中守卫必须区分「无命中」与「命令失败」。** `rg` 的退出码是 `0` = 有命中、`1` = 零命中、`>1` = 执行失败（非法正则、路径不存在、I/O 错误）。`! rg …` 和 `if rg …; then exit 1; fi` 都会把 `>1` 误判为通过，**不得**单独作为证据。裸 `rg …; rc=$?` 也不行——在 `set -e` 下零命中会让脚本在取 `$?` 前就退出。使用下列模板：

  ```bash
  if rg -n "<pattern>" <paths>; then
    echo "FAIL: forbidden pattern found"; exit 1
  else
    rc=$?
    if [ "$rc" -ne 1 ]; then echo "ERROR: rg failed with exit $rc"; exit "$rc"; fi
    echo "OK: zero hits"
  fi
  ```

- 「只允许 allowlist 命中」类守卫必须逐条比对固定 allowlist，并在任务记录中附命中 diff。
- 仅用于定位符号的 `rg` 必须注明「锚点定位用，非判据」。
- 本任务新增的 test fn / 场景过滤必须标 `(new)`，并确保其归属明确：lib（`monoengine_core`）内的 `#[cfg(test)]` 用例写 `-p monoengine --lib '<mod::path::tests::fn>'`；集成用例写 `-p monoengine --test <target>`，新 target 直接在 `tests/<target>.rs` 建文件（cargo 自动发现），并同步测试矩阵与 `docs/refactoring/integration.md` 覆盖矩阵。
- `cargo test --all` 不能替代任务指定用例（ER-04）；反过来，指定用例也不能替代计划完成前的全量门（见「完成判据」）。
- 依赖真实服务的用例必须在 Verification 里写清前置：Postgres 缺失会让相关用例 **panic**（不是跳过），Mailpit 在 lib 单测（`--lib`）侧是优雅跳过、在 `tests/` 集成侧是硬断言。把「跳过」当成「通过」属于幽灵验收。

### Task <ID>: <任务标题>

**Task type:** `<implementation | migration | removal | spike | audit | docs | release | handoff>`（G-11）

**Lifecycle / Acceptance:** `<pending | in-progress | blocked | done>` / `<空 | locally-accepted | remote-pending | complete>`（ER-04；两者正交，`done` 以 `complete` 为前提）

**Description:** `<要做什么、为什么、现实影响。一句话点明本卡唯一的行为轴。>`

**Out of scope:** `<逐项列出本卡明确不做的内容，每项标注状态：「由 <ID> 承接」/「尚未排期，重启条件 …」/「永久非目标，理由 …」。不得为了填表制造虚假承接关系（ER-03）。>`

**Current evidence:**

| 事实 | 证据 |
|---|---|
| `<当前实现或缺口>` | `<file:line / test / external repo@sha>` |

**Acceptance criteria:**

- [ ] `<用户可见或系统行为判据>`
- [ ] `<API/配置/schema/错误契约判据>`
- [ ] `<失败路径/边界条件判据>`
- [ ] `<文档/兼容/迁移同步判据>`

**Verification:**

- [ ] `<exact command>`
- [ ] `<exact command>`
- [ ] `<manual/sanitized evidence, if required>`

**Dependencies:** `<无 / 本计划 Task ID + 本卡消费的具体产物（接口、文件、测试） / 「依赖登记表」中的 DEP-ID>`（G-06）

**Deliverables:** `<docs / audit / spike / handoff 卡必填：产物文件清单（G-03 的产物范围登记位置）。代码卡写 N/A 或 Inherited。>`

**Implementation write set:** `<承载本卡行为的代码/测试/文档文件或目录。并发判定只看这一项：与并发在跑的卡不得相交，相交时只能补顺序边或合并到唯一集成卡>`（G-10）

**Release write set:** `<Inherited（= Cargo.toml 的 version 一处 + Cargo.lock）/ N/A（family child / no-release 卡）>`（不用于实现阶段并发分组；进入发布窗口后按 G-10 的 I–R / R–R 规则串行化）

**Files likely touched:** `<src/...>, <tests/...>, <config/...>, <docs/...>`（估计值；并发判定以 `Implementation write set` 为准）

**Docs and compatibility impact:** `<Inherited / 具体文件>`

**Rollback mode:** `<revert | forward-only | compensating | immutable-release>`（G-01）

**Migration and rollback:** `<N/A 或 sea-orm migration up/down、前滚步骤、数据不变量、恢复验证命令、用户影响；若 down 为空实现必须显式说明>`

**Security and privacy:** `<N/A 或 Cedar 策略、secret、路径、redaction、输入校验约束>`

**Performance budget:** `<N/A 或数据规模、复杂度、wall-clock/benchmark 断言>`

**Estimated scope:** `<S / M / L-exception:EX-<n>（仅限已登记的不可拆机械变更）>`（G-04；`XL` 永不允许作为开工态）

**Version increment:** `<patch（默认）| minor | major | N/A>`（ER-08）

**Release boundary:** `<independent（默认）| family child of REL-<n>（k/n）| family release point of REL-<n> | no-release（docs/audit/spike/handoff）>`（合并发布须先按 G-07 登记 `REL-*`）

**C/D coverage from:** `<self（自行执行 C 组）| <TASK-ID>（继承该发布点/收口点的 C 覆盖与其 D 组）>`（ER-04；`family child` 与 `no-release` 卡必填具体 ID，不得留空）

**Granularity:** `type=<Task type>; axis=<本卡唯一的行为轴>; recovery=<失败/撤回时的单一恢复动作与恢复后的自洽状态>; complete=<yes：实现+测试+文档同步都在本卡内>; self-contained=<yes：不读其它卡正文即可执行>; AC=<n>/<上限>[@EX-ID]; VER=<n>/<上限>[@EX-ID]; landing=<n>; prod-files=<n>; scope=<S|M|L-exception:EX-n>; deps=<none|TASK-ID,…|DEP-ID,…>; writeset=<no-overlap|序列化于 TASK-ID>; release=<independent|REL-n child|REL-n point|no-release>; split-from=<TASK-ID|N/A>; exception=<EX-ID[,EX-ID…]|N/A>`

字段与规则的对应：`type`→G-11，`axis`/`recovery`→G-01，`complete`→G-02，`AC`/`VER`→G-03（分母按 G-03 的 Task type 上限表取值：代码卡与 spike 为 8，`release` 为 12，docs/audit/handoff 为 20；ER-04 的强制门不计入）。**超限只有一种合规写法**：`AC=21/20@EX-01` —— 分子超过分母时必须紧跟豁免该列的 `EX-ID`，否则审计判为不达标；一张卡可同时需要多个豁免，`exception` 用逗号分隔并逐个说明所豁免的列。`landing`/`prod-files`/`scope`→G-04，`self-contained`→G-05，`deps`→G-06，`release`→G-07/G-08，`split-from`→G-09，`writeset`→G-10，`exception`→已登记的 `EX-*`。这一行是 `G-*` 的机器可核对摘要，ER-03 开工前逐字段核对；写不出来就说明卡还没拆干净。计划级汇总见「任务卡粒度审计表」。

## 测试矩阵

| 类别 | 必须覆盖 | Target / command |
|---|---|---|
| 单元 | `<纯逻辑、config parser、错误映射>` | `<cargo test -p monoengine --lib '<mod::tests>'>` |
| 集成（DB） | `<真实 Postgres + storage/migration 工作流>` | `<cargo test -p monoengine --lib '<mod::tests>'（test_db_connection + apply_migrations）>` |
| 集成（进程） | `<真实二进制黑盒、服务启动、配置解析>` | `<cargo test -p monoengine --test <target> -- --test-threads=1>` |
| CLI | `<子命令解析、三处注册、退出码、输出>` | `<cargo test -p monoengine --lib 'cli::tests'>` |
| HTTP API | `<路由、状态码、JSON schema、鉴权>` | `<cargo test -p monoengine --lib 'api::...'>` |
| 迁移 | `<up/down、old/new schema、数据迁移>` | `<cargo test -p monoengine --lib '<migration 相关 tests>'>` |
| 配置 | `<config validate / init / profile / secret 泄漏>` | `<cargo run -p monoengine -- ... config validate ...>` |
| Git 协议 | `<clone/fetch/push/shallow/protocol v2/LFS>` | `<scripts/git_protocol_smoke.sh 本地等价>` |
| 安全 | `<鉴权、Cedar 策略、secret、路径 traversal、错误 redaction>` | `<cargo test ...>` |
| 性能 | `<规模与预算>` | `<criterion / wall-clock>` |
| live/gated | `<真实外部服务或 provider（RustFS / Mailpit / SMTP）>` | `<docker compose profile 或 env gated command>` |

## 追溯表

| 任务 | 来源/证据 | monoengine 落点 | 文档/兼容动作 | 指定测试 |
|---|---|---|---|---|
| `<ID>` | `<file:line / issue / repo@sha>` | `<src/callisto / src/jupiter / src/api / src/main.rs ...>` | `<docs/...、config/config.toml、运行时 OpenAPI>` | `<-p monoengine --lib '<filter>' 或 -p monoengine --test <target>>` |

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
- [ ] `config/config.toml` 示例配置与注释已同步，或说明 `N/A`。
- [ ] 运行时 OpenAPI（`/api/openapi.json`）证据已取得，或说明 `N/A`（本仓无落盘 spec 文件）。
- [ ] `README.md` / `AGENTS.md` 相关工程约束已同步或登记漂移，或说明 `N/A`。
- [ ] `src/callisto/`、`src/jupiter/storage/` 与 `src/jupiter/migration/`（含 `migrations()` 注册列表）已同步，或说明 `N/A`。
- [ ] 新引入的 `docs/*.md` 引用全部指向真实存在的文件，或说明 `N/A`。
- [ ] `plan-long.md` 日期计划索引或 PT/SB 状态已同步，或说明 `N/A`。

## Review log

Result 只允许 `PASS` 或 `FAIL`。`FAIL` 必须列出 P0/P1 条目并在下一轮复审关闭；P2 可由具名责任人书面接受为 residual risk，但不改变本轮 `FAIL` 记录（ER-05）。

| Round | Scope | Result | P0/P1 | P2 处置 | Evidence |
|---|---|---|---|---|---|
| R1 | `<files/tasks>` | `<PASS / FAIL>` | `<条目与关闭状态>` | `<修复 / 具名接受人>` | `<test commands / 复审轮次>` |

## 非目标与延后项

| ID | 延后内容 | 原因 | 重启条件 | 承接位置 |
|---|---|---|---|---|
| DEFER-<PREFIX>-01 | `<内容>` | `<原因>` | `<何时重启>` | `<plan/PT/ADR>` |

## 完成判据

计划只有在以下条件全部满足后才能标记完成：

- [ ] 所有任务卡满足粒度规则 `G-*`：无未登记的 L 例外、无 XL 卡、无碎片卡、无未登记的合并发布例外、实现写集冲突均已消解；「任务卡粒度审计表」已填齐。
- [ ] 所有非延后任务的 acceptance criteria 已满足，且 `Lifecycle=done` **且** `Acceptance=complete`（ER-04）。任何停在 `remote-pending` 的卡都必须先取得其 D 组远端后置门的绿色证据。任何仍为 `blocked` 的任务都必须先解除阻塞（`blocked` → `in-progress` → 完成剩余动作 → `done`）或按 `DEFER-*` 正式延后，不得带着 `blocked` 通过完成门。
- [ ] 所有任务的 Verification 命令已运行并记录结果。
- [ ] **计划完成门（区别于每卡 focused gate）**：`cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test --all` 全绿，且 `cargo build`、`cargo build --tests` 均 0 错误 0 警告（`AGENTS.md` 强制）；不得新增 crate 级 `#[allow(...)]`。
- [ ] 必要的 docs/配置/错误契约/测试矩阵更新已完成。
- [ ] 必要的 migration、rollback、failure-recovery 验证已完成；每张卡的 `Rollback mode` 都已被实际验证或记录为不可验证的原因。
- [ ] 代码 review 最终结论为 `PASS`，P0/P1 全部关闭；仅 P2 residual risk 允许保留，且有具名接受人。
- [ ] 每次实际版本 bump 的版本面（ER-08 开工日核对的处数，当前为一处）已自洽、构建、提交、推送，并已创建和推送同名 `v<version>` tag；对应 GitHub Release 已通过 `gh` 使用人工编写的标准 release note 发布，未使用自动生成内容。
- [ ] 「修订历史」已记录成稿后的全部规范性变更（G-09）。
- [ ] `plan-long.md` 相关 PT/SB 状态或日期计划索引已同步，或明确 `N/A`。
