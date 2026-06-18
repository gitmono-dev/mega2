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

### 3. **refactoring/mail.md** — 邮件通知模块与 password_ref 迁移
- **目标**：完善一级 mail 模块，作为 config 的第一个真实 SecretRef 消费者
- **核心内容**：
  - Mail 模块的激活与现状（阶段 0/1 已完成）
  - Dispatcher 的生命周期管理
  - password 改为 password_ref 的迁移策略
  - 多 provider 扩展、模板系统、附件 metadata/删除/保留期治理、可靠性增强
- **关键前置**：config 阶段 5 的 SecretRef + resolver 实现
- **阶段范围**：0 - 5（共 6 个阶段，0/1 已完成）

### 4. **refactoring/notification.md** — 多渠道用户通知系统
- **目标**：将 notification 模块完整化，支持多渠道通知与 vault secret 集成
- **核心内容**：
  - Notification 模块现状（阶段 0 部分完成）
  - 渠道抽象（EmailChannel、InAppChannel、SlackChannel 等）
  - 用户偏好 API 表面（port mega DTOs）
  - 邮件作业与附件 metadata/删除/保留期管理面
  - Vault SecretRef 与多渠道凭据管理
  - 可靠性、可观测与模板系统
- **关键前置**：mail 模块的完整实现、vault 的加固完成
- **阶段范围**：0 - 5（共 6 个阶段）

### 5. **refactoring/integration.md** — 集成测试策略与执行方案

- **目标**：建立基于 Docker 容器的集成测试框架，验证配置、Vault、邮件、通知等核心模块的端到端功能
- **核心内容**：
  - Docker Compose 测试环境编排（PostgreSQL、Redis、SMTP、Vault）
  - 8 个主要集成测试场景（配置初始化、数据库连接、Secret 存储、邮件发送、通知触发等）
  - 测试覆盖矩阵与改进计划的对应关系
  - CI/CD 集成示例（GitHub Actions）
  - 故障排查与调试指南
- **关键依赖**：Docker、Docker Compose、PostgreSQL、Redis、Mailpit
- **验收标准**：覆盖所有改进计划的关键路径，确保各模块集成无误

### 6. **其他文档**
- **chat.md**：聊天/消息相关（本计划范围外）
- **protocol.md**：协议定义相关（本计划范围外）

---

## ⚙️ 文档使用指南

### 快速入门

1. **第一次接触本计划**？阅读 **general.md** 了解框架和规则
2. **想了解总体执行计划**？阅读 **README.md** 的"执行顺序"部分
3. **关心具体模块**？阅读对应的 **refactoring/contract.md / refactoring/config.md / refactoring/vault.md / refactoring/mail.md / refactoring/notification.md**
4. **想验证功能**？查看 **refactoring/integration.md** 的集成测试场景

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
  │   → 供 config 0b、vault A、mail 1、notification 0 使用
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

第一个真实消费者（第 3 轮）
  └─ mail 阶段 2（password_ref，依赖 config 5）
     → mail 阶段 1 的"可诊断处理"需要脱敏工具

多渠道完善（第 4 轮）
  └─ notification 阶段 1-3（依赖 mail 2 + vault A/B/C）
```

---

## 📋 执行顺序（按优先级与依赖关系）

### 第 0 阶段：独立前置

#### 独立前置 #1：日志脱敏工具 **[优先级：P0 独立]**
- **交付物**：`src/common/redaction.rs` 或 `src/config/redaction.rs`
- **职责**：脱敏 URL、token、secret、vault 信息
- **消费方**：config、vault、mail、notification
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

### 第 4 阶段：SecretRef 与第一个消费者

#### 4a. refactoring/config.md 阶段 5：SecretRef 基础设施 **[优先级：P2，关键]**
- **前置**：config 4 完成
- **消费者**：mail 阶段 2 直接依赖此
- **并行条件**：可与 vault E 并行
- **完成后**：resolver 就位，第一个可迁移凭据的迁移路径清晰

#### 4b. refactoring/vault.md 阶段 E：SecretRef 迁移 **[优先级：P2，与 config 5 协同]**
- **前置**：vault D 完成
- **协同**：与 config 5 一起完成，mail 作为共同的验证对象
- **完成后**：vault 侧的 resolver 就位

#### 4c. refactoring/mail.md 阶段 2：password_ref 迁移 **[优先级：P2]**
- **前置**：config 5 + vault E 都完成、mail 1 基本完成
- **完成后**：第一个 SecretRef 消费者验证完成，模式确立

---

### 第 5 阶段：多渠道与高级特性

#### 5a. refactoring/mail.md 阶段 3-5：Provider 扩展、可靠性等 **[优先级：P3-P4]**
- **前置**：mail 2 完成
- **并行条件**：可部分与 notification 0-1 并行

#### 5b. refactoring/notification.md 阶段 1-3：渠道抽象、API、安全加固 **[优先级：P2-P3]**
- **前置**：mail 2 完成、vault A/B/C 完成
- **并行条件**：可部分与 mail 3-5 并行
- **完成后**：多渠道通知系统基本成型

---

### 第 6 阶段：扩展与优化

#### refactoring/config.md 阶段 6-8：样例配置、Profile、热加载 **[优先级：P3-P4]**
- **前置**：阶段 5 完成

#### refactoring/vault.md 阶段 F-J：消费端加固、审计、权限等 **[优先级：P2-P4]**
- **前置**：阶段 E 完成

---

### 第 7 阶段：集成测试与验证

#### refactoring/integration.md 全景 **[优先级：P1-P2，贯穿全过程]**

集成测试应与各改进计划阶段并行执行，验证关键路径的端到端功能。参见 **refactoring/integration.md**：

- **Phase 1**：建立 Docker 容器环境（refactoring/config.md 0a/0b/1 并行）
- **Phase 2**：配置与数据库测试（refactoring/config.md 2/3 并行）
- **Phase 3**：Vault 与 CLI 测试（refactoring/config.md 4、refactoring/vault.md D/E 并行）
- **Phase 4**：邮件与通知测试（refactoring/mail.md 2、refactoring/notification.md 1-3 并行）
- **Phase 5**：高级功能测试（refactoring/config.md 8、测试覆盖矩阵全覆盖）
- **Phase 6**：CI/CD 集成与报告

关键验收标准：
- ✅ 8 个主要集成测试场景全部通过
- ✅ 配置、Vault、邮件、通知全链路正常
- ✅ 错误诊断与脱敏功能生效
- ✅ CLI 工作流完整可用
- ✅ GitHub Actions CI 集成完成

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
        (vault C/D 可并行)  config 4/5 → mail 2 → notification 1-3
```

### 不能跳过的步骤

- ❌ 不能在 vault A 前启动 vault 的其他工作（都需要脱敏工具）
- ❌ 不能在 config 5 前启动 mail 2（需要 resolver）
- ❌ 不能在 mail 2 前启动 notification 阶段 3（需要 password_ref）
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
3. mail 阶段 2
4. notification 阶段 1-3

### P3 - 后续优化与扩展
1. mail 阶段 3-4
2. config 阶段 6
3. notification 阶段 4-5
4. vault 阶段 F-I

### P4 - 长期、可选或低优先级
1. mail 阶段 5
2. config 阶段 7-8
3. vault 阶段 G/J

---

## 📖 如何使用这个文档

1. **快速导航**：查看"文档概览"了解每个文档的目标
2. **依赖关系**：查看"文档之间的依赖关系"理解全景
3. **执行计划**：按照"执行顺序"的阶段逐步推进
4. **约束检查**：在每个阶段开始前检查"关键约束"是否满足

---

## 📝 最后一次更新

- **日期**：2026-06-14
- **更新内容**：新增前置依赖说明、清晰的执行顺序、跨模块协调点
- **涵盖文档**：refactoring/config.md、refactoring/vault.md、refactoring/mail.md、refactoring/notification.md
