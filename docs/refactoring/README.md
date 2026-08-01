# Monoengine 改进计划文档索引

本目录包含 monoengine 的改进计划文档体系。这些文档构成一个相互联系的系统，描述了 contract 边界、配置管理、敏感凭据存储、邮件通知和用户通知的完整改进方案。

> **通用治理规范**：所有改进计划文档必须遵循 **`general.md`** 中定义的共同结构、约束、版本管理和评审标准。在执行改进时，请首先查阅 `general.md` 了解共同需求。

## 📚 文档概览

### 0. **general.md** — 通用治理与组织规范

- **目标**：定义所有改进计划文档的统一组织方式、共同约束、治理规则和执行标准
- **核心内容**：
  - 文档组织体系与分层结构
  - 所有文档的必需部分与禁止内容
  - 跨文档协调的共同规则
  - 执行时的共同需求与验收标准
  - 团队协作与决策流程
  - 文档版本与变更管理
  - 关键术语与定义
- **作用**：基础框架，所有其他文档的执行基准

### 1. **refactoring/config.md** — 配置模块的模块化与 SecretRef 实现
- **目标**：将配置系统从 `src/common/config.rs` 提升为一级 `src/config/` 模块，实现敏感凭据的 vault 存储和运维命令
- **核心内容**：
  - 配置模块结构拆分（8 个阶段）
  - 敏感凭据分类（引导配置、早期运行时依赖、可迁移凭据）
  - SecretRef 设计与 resolver 实现
  - CLI 两阶段加载（LoadMode）框架
  - Profile、热加载、集中校验等高级特性
- **关键前置**：日志脱敏工具、CLI LoadMode 框架设计
- **阶段范围**：0a - 8（共 8 个阶段）

### 1a. **refactoring/contract.md** — Contract 边界归并
- **目标**：将 API 数据契约、Git 协议、Vault 与权限策略相关代码统一归入 `src/contract/`
- **核心内容**：
  - `api_model` → `contract::api`
  - `git_protocol` → `contract::git_protocol`
  - `vault` → `contract::vault`
  - `saturn` + `api::guard` → `contract::policy`
- **关键前置**：无；属于结构性路径迁移
- **阶段范围**：0 - 2（结构归并、文档同步、常规门禁）

### 2. **refactoring/vault.md** — Vault 安全加固与凭据迁移
- **目标**：加固 vault 安全（fail-closed、脱敏、权限等），为凭据迁移提供坚实基础
- **核心内容**：
  - Vault 现状分析与安全风险评估
  - 分阶段加固计划（A-J 共 10 个阶段）
  - 最小 DB/Vault bootstrap 设计
  - CLI 运维命令实现
  - Secret 轮换、审计、权限管理
  - 备份恢复方案
- **关键前置**：日志脱敏工具、与 config 的 CLI 协同设计
- **阶段范围**：A - J（共 10 个阶段）

### 3. **refactoring/mail.md** — 本仓 SMTP 邮件模块（已废止）
- **状态**：废止。monoengine 不再实现、配置或运行 SMTP 邮件投递（ADR-WA-08）。
- **现行事实源**：[`website-mail.md`](./website-mail.md)（产品邮件经 website 内部 API）；本文件仅保留废止说明与历史索引。
- **不要**：把 `mail.md` 当作活的 SecretRef / SmtpMailer / Mailpit 设计文档。

### 4. **refactoring/notification.md** — 多渠道用户通知系统
- **目标**：通知编排（in-app / 可选 Slack/webhook）与用户偏好；产品邮件经 website-mail 客户端转发。
- **核心内容**：
  - primary = `in_app`；`delivery_mode=email` 映射为「写 in-app + POST website」
  - 无 `EmailChannel` / `email_jobs` / 本仓 SmtpMailer
  - website-mail 客户端配置（`notification.website_mail_*`）
  - 触发器与偏好 API 表面
- **关键前置 / 契约**：[`website-mail.md`](./website-mail.md)；Slack/webhook 凭据可继续用 SecretRef
- **长期收尾**：webhook/slack 完善、build 完成触发器、多实例矩阵（见 `plan-long.md` PT-09）

### 4a. **refactoring/orbit.md** — Orbit 依赖重构（项目引用 → API 依赖）
- **目标**：把 monoengine 对 orbit **实现 crate** 的 `path` 项目引用，重构为只依赖 `orbit-api`（纯 API/契约 crate），把唯一的具体构造点通过依赖注入下沉到组合根 / 瘦二进制边界
- **核心内容**：
  - 唯一实现 crate 触点定位（`Storage::new` 中的 `ObjectStorageFactory::build`）与 18 处已是 `orbit_api` 的引用
  - 注入 `MegaObjectStorageWrapper` 值的两段式策略（seam 隔离 + lib/bin 拆分）
  - 单一二进制 crate 形态对"真正移除重量级依赖"的约束（需阶段 3 拆 `monoengine-core`）
  - 待决策项：是否拆 crate、impl 落点、注入形态、orbit-api 是否发布
- **关键前置**：无（结构性依赖治理）；与 config（ObjectStorageConfig 来源）、vault（启动顺序）协同
- **阶段范围**：0 - 3（共 4 个阶段）
- **状态（2026-06-19）**：✅ 已完整实现。已拆为 `monoengine-core`（lib，仅 `orbit-api`）+ `monoengine`（bin，注入 `orbit` 实现）；`cargo tree -p monoengine-core` 无 `object_store`/云 SDK。唯一开放项：是否发布 `orbit-api` 为带版本依赖

### 5. **refactoring/integration.md** — 集成测试策略与执行方案

- **目标**：基于 Docker Compose 的集成测试框架，验证配置、Vault、会话、通知编排与 git-cli 等端到端路径
- **核心内容**：
  - Docker Compose 测试栈（PostgreSQL、Redis、mailpit、rustfs、git-cli、`website-next`；`-p monoengine-it`）
  - 黑盒 target：`integration_vault` / `integration_website_auth` / `integration_git_cli`
  - **mailpit 消费方 = website IT**；本仓**无** SmtpMailer→Mailpit 成功门
  - 覆盖矩阵与 CI（`config-validation.yml` 等）
- **关键依赖**：Docker、Docker Compose、PostgreSQL、Redis；website 会话/邮件 IT 另需 `website-next`
- **验收标准**：见 `integration.md` / `test-infra.md` 现行矩阵；不以本仓 SMTP 为门

### 6. **其他文档**
- **website-auth.md** / **website-mail.md**：Website 会话与产品邮件契约（见 `plan-20260731.md`）
- **protocol.md**：协议定义相关（本计划范围外）
- 本仓 Campsite 风格 chat/Notes 产品面已退场；不再维护独立 chat 改进文档

---

## ⚙️ 文档使用指南

### 快速入门

1. **第一次接触本计划**？阅读 **general.md** 了解框架和规则
2. **想了解总体执行计划**？阅读 **README.md** 的"执行顺序"部分
3. **关心具体模块**？阅读对应的 **refactoring/contract.md / refactoring/config.md / refactoring/vault.md / refactoring/notification.md / refactoring/website-mail.md**（`mail.md` 已废止）
4. **想验证功能**？查看 **refactoring/integration.md** / **refactoring/test-infra.md** 的集成测试场景

### 角色指南

| 角色 | 首先阅读 | 然后关注 |
|------|---------|--------|
| **项目经理** | README.md、general.md | 优先级、依赖关系、风险清单 |
| **工程师（contract）** | refactoring/contract.md、general.md | 模块边界、路径迁移、旧路径清理 |
| **工程师（config）** | general.md、refactoring/config.md | 前置依赖、硬约束、验收标准 |
| **工程师（vault）** | general.md、refactoring/vault.md | 与 config 的协同点、bootstrap 拆分 |
| **QA/测试** | refactoring/integration.md、general.md | 集成测试场景、验收标准 |
| **架构评审** | general.md、各模块文档 | 硬约束、多维评估、跨模块风险 |

### 常见问题

**Q: 某个阶段需要多少时间？**  
A: 文档中不包含时间估计。请根据团队的实际速度进行评估。

**Q: 这个改进与其他模块有什么依赖关系？**  
A: 查看对应文档的"前置依赖矩阵"或 README.md 的"执行顺序"部分。

**Q: 代码现状与文档不符，应该相信谁？**  
A: 检查文档顶部的"事实校准"日期。如果距今超过 1 个月或代码有重大变化，请提出来更新文档。

**Q: 能否跳过某个前置工作？**  
A: 查看对应的"前置依赖矩阵"。标记为"硬约束"的不能跳过；标记为"可选"的可以评估跳过。

**Q: 如何添加新的改进计划？**  
A: 参照 general.md 的结构创建新文档，确保包含所有必需部分。

---

## 🔗 文档之间的依赖关系

```
独立前置（第 0 轮）
  ├─ 日志脱敏工具
  │   → 供 config 0b、vault A、notification 使用
  │
  └─ CLI LoadMode 框架协同设计（config + vault 团队）
      → config 阶段 2 与 vault 阶段 D 都依赖此

核心改进计划（第 1-2 轮）
  ├─ config 阶段 0a → 0b（使用脱敏工具）
  ├─ vault 阶段 A（使用脱敏工具）
  │   → 这两个可以并行进行
  │
  ├─ config 阶段 1 → 2（使用 CLI LoadMode）
  ├─ vault 阶段 B（最小 bootstrap）
  │   → vault B 应在 config 3 前或同期完成
  │
  ├─ config 阶段 3（依赖 vault B）
  ├─ vault 阶段 C → D（使用 CLI LoadMode）
  │   → 这些可以并行进行
  │
  └─ config 阶段 4 → 5（SecretRef + resolver）

SecretRef 消费者（第 3 轮，现行）
  └─ redis.url / object_storage / notification.website_mail_bearer_ref 等
     （本仓 SMTP/`mail.password_ref` 路线已废止，见 mail.md + website-mail.md）

通知编排（第 4 轮）
  └─ notification：in-app / Slack / webhook + website-mail 客户端
     （不依赖本仓 mail 模块）
```

---

## 📋 执行顺序（按优先级与依赖关系）

### 第 0 阶段：独立前置

#### 独立前置 #1：日志脱敏工具 **[优先级：P0 独立]**
- **交付物**：`src/common/redaction.rs` 或 `src/config/redaction.rs`
- **职责**：脱敏 URL、token、secret、vault 信息
- **消费方**：config、vault、notification（含 website-mail client）
- **验收**：可被多个模块导入使用，单元测试全覆盖

#### 独立前置 #2：CLI LoadMode 框架设计 **[优先级：P0 独立，config + vault 协同]**
- **交付物**：共同的 `LoadMode` enum 定义与文档
- **设计方**：config + vault 团队
- **应用方**：config 阶段 2、vault 阶段 D

---

### 第 1 阶段：基础设施加固

#### 1a. refactoring/config.md 阶段 0a：纯结构拆分 **[优先级：P1]**
- **前置**：无
- **并行条件**：可与其他工作并行
- **完成后**：repo 结构就位，路径可以逐步迁移

#### 1b. refactoring/config.md 阶段 0b：错误模型 + 脱敏工具 **[优先级：P0 with 脱敏工具]**
- **前置**：脱敏工具已完成
- **并行条件**：可与 vault A 并行
- **完成后**：redaction 工具就位，config 可向脱敏输出转变

#### 1c. refactoring/vault.md 阶段 A：安全止血 **[优先级：P0]**
- **前置**：脱敏工具已完成
- **并行条件**：可与 config 0b 并行
- **完成后**：root token 不再泄露，fail-closed 判定逻辑就位

---

### 第 2 阶段：消费端路径与 bootstrap 拆分

#### 2a. refactoring/config.md 阶段 1：路径迁移 **[优先级：P1]**
- **前置**：config 0a 完成
- **并行条件**：可独立进行
- **完成后**：所有调用方从 `common::config` 改为 `config::*`

#### 2b. refactoring/vault.md 阶段 B：最小 bootstrap **[优先级：P1，关键约束]**
- **前置**：vault A 完成
- **并行条件**：可与 config 1 并行，但应在 config 3 前完成
- **后置依赖**：config 阶段 3 直接依赖此
- **完成后**：可以不依赖完整 AppContext 进行 vault 操作

#### 2c. refactoring/config.md 阶段 2：CLI LoadMode + config 命令 **[优先级：P1，需要协同]**
- **前置**：CLI LoadMode 框架设计完成、config 1 完成
- **协同**：与 vault D 共用 LoadMode 框架
- **并行条件**：可与 vault C 并行
- **完成后**：`config init/validate/secret ref` 命令可用

#### 2d. refactoring/vault.md 阶段 C：收窄 interface **[优先级：P1]**
- **前置**：vault A 完成
- **并行条件**：可与 config 2 并行
- **完成后**：vault 的对外接口更安全

---

### 第 3 阶段：运维命令与 SecretRef 实现

#### 3a. refactoring/config.md 阶段 3：最小 bootstrap + core_key 加固 **[优先级：P1]**
- **前置**：vault B 完成，config 2 完成
- **并行条件**：可与 vault D 并行
- **完成后**：最小 bootstrap 能力在 config 层实现，fail-closed 一致化

#### 3b. refactoring/vault.md 阶段 D：CLI LoadMode 与运维命令 **[优先级：P1，需要协同]**
- **前置**：CLI LoadMode 框架设计完成、vault B/C 完成
- **协同**：与 config 2 共用 LoadMode 框架
- **并行条件**：可与 config 3 并行
- **完成后**：`config secret ref/set/check` 命令可用

#### 3c. refactoring/config.md 阶段 4：secret set/check 命令 **[优先级：P2]**
- **前置**：config 3、vault D 完成
- **并行条件**：可与 vault E 并行
- **完成后**：运维命令完整链路形成

---

### 第 4 阶段：SecretRef 与现行消费者

#### 4a. refactoring/config.md 阶段 5：SecretRef 基础设施 **[优先级：P2，关键]**
- **前置**：config 4 完成
- **消费者**：`redis.url`、对象存储凭据、`notification.website_mail_bearer_ref` 等（**不再**是本仓 `mail.password_ref`）
- **并行条件**：可与 vault E 并行
- **完成后**：resolver 就位，可迁移凭据的路径清晰

#### 4b. refactoring/vault.md 阶段 E：SecretRef 迁移 **[优先级：P2，与 config 5 协同]**
- **前置**：vault D 完成
- **协同**：与 config 5 一起完成；验证对象为现行 SecretRef 字段（非 SMTP mail）
- **完成后**：vault 侧的 resolver 就位

#### 4c. ~~refactoring/mail.md 阶段 2：password_ref~~ **[已废止]**
- 本仓 SMTP / `[mail]` / `mail.password_ref` 已按 ADR-WA-08 移除。
- 现行契约：[`website-mail.md`](./website-mail.md)；废止页：[`mail.md`](./mail.md)。

---

### 第 5 阶段：通知编排（无本仓 SMTP）

#### 5a. ~~refactoring/mail.md 阶段 3-5~~ **[已废止]**
- 不再扩展本仓 SmtpMailer / provider / outbox。

#### 5b. refactoring/notification.md：in-app / Slack / webhook + website-mail **[优先级：P2-P3]**
- **前置**：config/vault SecretRef（渠道凭据）；website-mail 契约（ADR-WA-08）
- **不依赖**：本仓 mail 模块
- **完成后**：产品邮件走 website；本仓保留非邮件渠道与编排
- **长期收尾**：见 `plan-long.md` PT-09

---

### 第 6 阶段：扩展与优化

#### refactoring/config.md 阶段 6-8：样例配置、Profile、热加载 **[优先级：P3-P4]**
- **前置**：阶段 5 完成

#### refactoring/vault.md 阶段 F-J：消费端加固、审计、权限等 **[优先级：P2-P4]**
- **前置**：阶段 E 完成

---

### 第 7 阶段：集成测试与验证

#### refactoring/integration.md 全景 **[优先级：P1-P2，贯穿全过程]**

集成测试应与各改进计划阶段并行执行。参见 **refactoring/integration.md** 与 **refactoring/test-infra.md**：

- **Phase 1**：Docker Compose 数据面（postgres/redis/…）
- **Phase 2**：配置与数据库 / Vault CLI（`integration_vault`）
- **Phase 3**：会话同栈（`website-next` + `integration_website_auth`）
- **Phase 4**：通知编排（in-app / website-mail client mock；**无**本仓 SmtpMailer→Mailpit 门）
- **Phase 5**：git-cli / 协议矩阵与高级覆盖
- **Phase 6**：CI（`config-validation.yml` 等）

关键验收标准：
- ✅ 现行黑盒 target 与矩阵路径通过（见 `integration.md`）
- ✅ 配置、Vault、会话、通知编排关键路径正常
- ✅ 错误诊断与脱敏功能生效；`MEGA_MAIL__*` / `[mail]` 硬拒
- ✅ CLI 工作流完整可用
- ✅ GitHub Actions CI 集成完成；mailpit 仅 website IT 可选消费

---

## 🔴 关键约束与必须完成的前置

### 必须先做（不能推迟）

1. **脱敏工具** — 是 vault A 的**必要条件**，P0 阶段 A 无法开始
2. **CLI LoadMode 框架设计** — config 2 和 vault D 都需要，必须协同
3. **vault B 在 config 3 前完成** — config 3 直接依赖 vault B 的改造结果

### 不能并行的关键路径

```
脱敏工具 → vault A
           ↓
        vault B → config 3
           ↓        ↓
        (vault C/D 可并行)  config 4/5 → 现行 SecretRef 消费者
                                      → notification（website-mail，非本仓 SMTP）
```

### 不能跳过的步骤

- ❌ 不能在 vault A 前启动 vault 的其他工作（都需要脱敏工具）
- ❌ 不能在 config 5 前把生产凭据迁入 SecretRef（需要 resolver）
- ❌ 不能恢复本仓 SMTP/`[mail]` 作为 notification 依赖（ADR-WA-08）
- ❌ 不能分别实施 config 2 和 vault D 的 CLI 改造（必须协同）

---

## 🎯 按优先级排序（所有优先级的任务）

### P0 - 必须先做
1. 脱敏工具（独立前置）
2. CLI LoadMode 框架设计（协同前置）
3. config 阶段 0b + vault 阶段 A

### P1 - 高优先级，紧随 P0
1. config 阶段 1/2/3
2. vault 阶段 B/C/D

### P2 - 中等优先级，第一批功能完整
1. config 阶段 4/5
2. vault 阶段 E
3. notification（website-mail 客户端 + in-app/Slack/webhook；**无本仓 mail 阶段**）

### P3 - 后续优化与扩展
1. config 阶段 6
2. notification 长期收尾（PT-09：webhook/slack/多实例等）
3. vault 阶段 F-I

### P4 - 长期、可选或低优先级
1. config 阶段 7-8
2. vault 阶段 G/J

---

## 📖 如何使用这个文档

1. **快速导航**：查看"文档概览"了解每个文档的目标
2. **依赖关系**：查看"文档之间的依赖关系"理解全景
3. **执行计划**：按照"执行顺序"的阶段逐步推进
4. **约束检查**：在每个阶段开始前检查"关键约束"是否满足

---

## 📝 最后一次更新

- **日期**：2026-08-01（历史：2026-06-14 起稿；2026-06-19 orbit；2026-06-23 Scope 内交付 v0.1.49）
- **更新内容**：DOC-01 收口——`mail.md` 标废止；执行顺序/依赖图去掉本仓 SMTP/`password_ref` 主线，改为 website-mail + 现行 SecretRef 消费者；integration 验收对齐 `test-infra.md`（mailpit = website IT）。
- **涵盖文档**：refactoring/config.md、refactoring/vault.md、refactoring/mail.md（废止）、refactoring/website-mail.md、refactoring/website-auth.md、refactoring/notification.md、refactoring/orbit.md、refactoring/integration.md、refactoring/test-infra.md、refactoring/contract.md
