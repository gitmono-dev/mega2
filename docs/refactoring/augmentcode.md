# Augment Code 功能对比与 monoengine 增量计划

更新时间：2026-06-21

本文基于 Augment Code 官网与公开文档的当前功能快照，对比
monoengine 已有实现和 `docs/refactoring/` 下的计划文档，给出
monoengine 若要覆盖同类“企业级 AI 软件工程平台”能力需要新增的功能。

## 结论

Augment Code 当前不只是一个补全工具，而是围绕代码库上下文、可复用专家、
事件触发、远程沙箱、MCP 工具、代码审查、CLI 自动化和组织知识构建的一套
Agent 平台。其核心产品线可以拆成三层：

1. **Context Engine**：为代码库、文档、提交历史、规范、runbook 和外部知识
   建立可被 Agent 和 IDE/CLI 消费的语义上下文。
2. **Agent Runtime / Cosmos**：把 Expert、Trigger、Capability、Environment、
   Session、Artifact、Secret、Visibility 等概念组合成可调度的自动化运行时。
3. **SDLC 自动化应用**：在 PR 作者、Pair Review、Deep Code Review、风险分析、
   测试、工单/告警处理、CI 失败修复、定时报表等场景中复用上述运行时。

monoengine 当前已经有 Git/CL、代码审查评论、reviewer 策略、merge queue、
build trigger、webhook、artifact、notification、chat、Vault/Secret、Cedar policy、
bot token 等重要地基，但这些能力还停留在“代码托管与协作服务”层面。缺口集中在：

- 没有 Agent/Expert/Session/Run/Tool/Memory/Trigger 的一等领域模型。
- 没有面向代码库和组织知识的语义 Context Engine。
- 没有可隔离执行任务的 Agent Runtime、沙箱、checkpoint、human-in-the-loop。
- 没有 MCP client/server、外部工具能力注册、JSONLogic 触发器和 slash command。
- 没有 AI 代码审查、PR 作者、风险分析、测试专家等 SDLC 自动化应用。
- 现有 webhook/build trigger/notification/chat/artifact/bot/policy 尚未被统一成
  Agent 工作流骨架。

因此，monoengine 的增量方向不应先从“接一个 LLM API”开始，而应先把已有协作、
事件、权限、密钥、工件和通知模块收束为 Agent 平台的领域边界，再逐步接入模型
和远程执行。

## Augment Code 功能拆解

### 1. Agent 与 Agent Auto

Augment 的 Agent 面向端到端软件工程任务：理解代码、制定计划、修改文件、执行
命令、运行测试、创建文档，并在关键节点展示变更、命令输出和 checkpoint。
Agent Auto 更偏向自主执行，普通 Agent 则会在外部集成或高风险命令前暂停等待
用户确认。

monoengine 对应缺口：

- 需要 `agent_session` 作为用户可见会话，记录 prompt、状态、执行环境、可见性、
  触发来源、关联 CL/issue/build/artifact。
- 需要 `agent_run` / `agent_step` 记录每次执行、工具调用、命令输出、补丁、测试、
  checkpoint、失败原因和重试。
- 需要 human-in-the-loop 机制，让敏感工具调用、写操作、secret 访问、外部 API
  调用进入待审批状态。

### 2. Cosmos：Expert、Trigger、Capability、Environment

Augment Cosmos 将 Agent 抽象成可复用的 Expert，并通过事件源触发运行。公开文档中
的事件源包括 GitHub、Slack、Linear、PagerDuty、webhook、cron；Expert 可绑定
Environment、Capability、Trigger、Skill、Visibility、Session 和 Files。

monoengine 对应缺口：

- 需要 `agent_experts` 存储可复用专家模板：说明、系统提示词、模型策略、默认工具、
  触发条件、可见性、预算和审计规则。
- 需要 `agent_capabilities` 统一描述内部工具和外部工具，例如 CL 评论、code review、
  merge queue、artifact 上传、notification 发送、webhook 调用、MCP 工具。
- 需要 `agent_triggers` 承接 Git push、CL 事件、issue/comment、build failure、
  webhook、cron、chat mention、slash command，并支持 JSONLogic 级别过滤。
- 需要 `agent_environments` 描述执行位置：本机 worker、容器、远程 VM、客户私有云，
  以及资源限制、网络策略、secret 注入规则。

### 3. Context Engine

Augment Context Engine 向 Agent、CLI、IDE 和第三方 AI 工具提供代码库语义搜索、
关系理解、提交历史、规范文档、runbook、外部网站和对象存储等上下文，并通过 MCP
和 SDK 暴露能力。

monoengine 对应缺口：

- 需要独立的 `context_engine` 或 `agent_context` 领域，而不是只依赖 Git 文件读取。
- 需要索引对象覆盖 repo 文件、symbols、提交、CL、code review 评论、issue、
  chat、build log、artifact manifest、docs/refactoring 计划和外部文档。
- 需要增量索引：Git push、CL 更新、文档变化、artifact 上传、webhook 事件后刷新。
- 需要检索 API：按 repo/path/symbol/issue/CL/build/session 召回，并返回可审计引用。
- 需要可选向量/全文混合检索与权限过滤，避免 Agent 看到未授权 repo 或 secret。

### 4. Auggie CLI 与自动化

Auggie CLI 是面向终端和 CI/CD 的 Agent 入口，可在本地或流水线中分析代码、修改代码、
执行工具、处理 PR/build 反馈、triage issue/alert，并支持非交互输出模式。

monoengine 对应缺口：

- 当前 CLI 仅注册 `service`、`chat-migrate`、`config`，还没有 `agent`、`expert`、
  `context`、`workflow`、`mcp` 等命令。
- 需要增加 `mono agent run`、`mono agent resume`、`mono agent logs`、
  `mono expert list`、`mono context search`、`mono mcp serve` 等命令。
- 需要支持 CI 模式：非交互、结构化输出、退出码语义、最大预算、只读/可写策略。

### 5. AI Code Review

Augment 的代码审查能力强调 GitHub PR 原生集成、自动/手动/禁用三种触发模式、
自定义审查指南、MCP 上下文、访问控制和分析面板。它聚焦高信号问题，例如 bug、
安全、正确性和跨系统影响。

monoengine 对应缺口：

- 现有 `code_review` 已有 thread/anchor/position/comment 模型，但还没有 AI reviewer。
- 需要把 reviewer 策略、Cedar policy、code_review thread 和 Agent Runtime 连接起来：
  自动触发审查、生成 inline comment、回复用户追问、标记风险等级。
- 需要 `review_guidelines`：仓库级规则、路径级规则、语言规则、团队偏好。
- 需要 `review_runs` 或复用 `agent_run` 存储审查输入、上下文引用、模型版本、输出、
  被接受/驳回反馈。
- 需要分析面板：命中率、误报率、修复率、审查耗时、风险类型分布。

### 6. MCP、集成与外部工具

Augment 支持 MCP 服务器配置，并将外部系统如 CircleCI、MongoDB、Redis 等上下文
流入 Agent 运行。Cosmos 也通过 Slack/GitHub/Jira/CI 等集成把事件和工具能力接入
工作流。

monoengine 对应缺口：

- 需要 MCP client：Agent 运行时可调用外部 MCP tool，并将权限、secret、日志纳入审计。
- 需要 MCP server：向外部 AI 工具暴露 monoengine 的 repo/CL/issue/build/artifact
  搜索和操作能力。
- 需要集成注册中心：区分系统集成、组织集成、用户 OAuth、bot/service account。
- 需要 loop guard：避免机器人评论、workflow_run、webhook 回调反复触发自身。

### 7. Secrets、Artifacts、Service Account

Augment Cosmos 将 secret 注入 Expert VM，支持私有和共享 scope，并会从日志中剥离
敏感值；Artifact 则作为会话输出，关联 PR、branch、ticket、链接和报告。

monoengine 对应缺口：

- 现有 Vault 可作为 secret 地基，但缺少 Agent 级 secret scope、运行时注入和日志脱敏。
- 现有 artifact service 可处理对象下载/上传，但没有和 agent_session/run 绑定。
- 现有 bot token 可作为服务身份地基，但缺少 workflow/service account 的归因模型、
  权限模板和审计视图。

## monoengine 当前实现地基

以下判断以源码为准；部分 `docs/refactoring/` 文档仍描述早期状态。

| 领域 | 当前实现 | 与 Augment 类能力的差距 |
| --- | --- | --- |
| API 聚合 | `src/api/api_router.rs:28` 起已挂载 CL、reviewer、merge_queue、artifacts、code_review、build_trigger、webhook、bot、chat 等 router。 | 这些 router 还不是 Agent 工具注册表的一部分，缺少统一能力声明、权限、审计和工具调用协议。 |
| 应用上下文 | `src/context/mod.rs:14` 起构建 Storage、Vault、Config、Redis、notification dispatcher。 | 可作为 Agent Runtime 依赖注入入口，但还没有运行时、队列、checkpoint、worker。 |
| Storage/Service | `src/jupiter/storage/mod.rs:210` 起创建 code_review、build_trigger、bots、webhook、audit、chat 等 storage/service。 | 地基丰富，但领域边界仍按业务模块分散，缺少 Agent 聚合层。 |
| CLI | `src/commands/mod.rs:33` 起仅有 `service`、`chat-migrate`、`config`。 | 无 Auggie 类 CLI、CI 自动化入口、MCP server 命令。 |
| Chat | `src/api/router/chat_router.rs:26` 起有 channel/message/reaction/attachment/read API；`src/chat/service/channel_chat.rs:227` 处已有通知 TODO。 | 可作为 human-in-the-loop 和 slash command 界面，但实时事件默认 no-op，mention/agent 触发、外部集成未完成。 |
| Code Review | `src/api/router/code_review_router.rs:17` 起有 inline thread/reply/resolve/reopen/delete；`src/jupiter/service/code_review_service.rs:53` 起处理锚点与位置。 | 当前是人类/系统评论基础设施，不是 AI review 产品；缺少自动触发、审查准则、风险分级、反馈闭环。 |
| Reviewer/Policy | `src/jupiter/service/reviewer_service.rs:1` 起支持 Cedar 中的 mandatory reviewer 解析；`src/contract/policy/guard/cedar_guard.rs:23` 仍有开发期 all-permit TODO。 | 可扩展成 Agent 权限和审批策略，但当前覆盖面窄，资源解析仍硬编码。 |
| Merge Queue | `src/api/router/merge_queue_router.rs:18` 起支持 add/remove/list/status/retry/stats/cancel；`src/jupiter/service/merge_queue_service.rs:153` 起有 processor 控制。 | 可作为 Agent 的合并工具，但缺少“运行测试、判断风险、自动修复、重新排队”的工作流。 |
| Build Trigger | `src/ceres/build_trigger/mod.rs:49` 注册 GitPush、Manual、Retry、WebEdit、BuckFileUpload；`Webhook` 和 `Schedule` handler 仍保留未来扩展。 | 已接近自动化事件层，但不是通用 trigger engine，也没有 JSONLogic、cron、agent run 创建。 |
| Webhook | `src/api/router/webhook_router.rs:23` 起支持创建/列表/删除出站 webhook；`src/jupiter/service/webhook_service.rs:114` 起异步分发并校验 URL/HMAC。 | 当前主要是出站通知，不是入站事件源；需要反向接入 Agent trigger。 |
| Notification | `src/notification/service.rs:65` 起注册 email 与 in-app；`src/notification/channels/mod.rs:1` 标注 Slack/webhook 为未来方向。 | 可承载 Agent 运行状态通知，但缺少 Slack/Teams/PagerDuty、用户审批交互和 Agent 事件模板。 |
| Artifact | `src/api/router/artifacts_router.rs:35` 起支持 discovery、object、batch、commit、fallback upload；`src/jupiter/service/artifact_service.rs:46` 起提供对象服务。 | 可承载 Agent 输出，但缺少 session/run 关联、搜索、可视化报告和生命周期策略。 |
| Bot/Service Identity | `src/api/router/bot_router.rs:70` 起支持 bot 安装和 token；`src/jupiter/storage/bots_storage.rs:237` 起生成 token hash。 | 可作为 workflow 身份地基，但缺少服务账号归因、权限模板、workflow audit 和 OAuth/user 集成。 |
| Vault/Secret | `src/context/mod.rs:55` 起 notification 会从 Vault 解析 mail password；`src/contract/vault/` 提供 VaultCore 集成。 | 缺少 Agent secret scope、per-run 注入、日志脱敏、用户私有 secret 覆盖共享 secret。 |
| 计划文档 | `docs/refactoring/README.md` 当前主线仍是 config/vault/mail/notification/integration；`docs/refactoring/chat.md` 把外部集成和 OpenAI 明确排除在聊天计划外。 | 还没有 Agent/Cosmos/Context Engine 方向的系统计划，本文件应作为后续 refactoring 入口。 |

## 需要新增的功能

### P0：Agent 领域模型与运行记录

新增模块建议：

- `src/agent/domain.rs`
- `src/agent/service/`
- `src/agent/runtime/`
- `src/agent/tools/`
- `src/agent/context/`
- `src/agent/triggers/`
- `src/api/router/agent_router.rs`
- `src/commands/agent.rs`

核心表建议：

- `agent_experts`：专家模板、说明、visibility、默认模型策略、默认 capability 集合。
- `agent_sessions`：用户可见会话，关联 repo、CL、issue、chat、build、artifact。
- `agent_runs`：一次执行实例，记录触发源、状态、预算、环境、取消/重试信息。
- `agent_steps`：工具调用、命令、模型响应、审批点、checkpoint、错误。
- `agent_capabilities`：内部工具与外部 MCP 工具定义。
- `agent_trigger_specs`：事件源、过滤条件、目标 expert、loop guard。
- `agent_artifacts`：run/session 输出与现有 artifact set/object 的关联。
- `agent_memories`：组织、仓库、expert、用户层面的可审计记忆。
- `agent_secrets`：secret 引用，不存明文，绑定 Vault path、scope、注入策略。

验收标准：

- 能创建 Expert 和 Session。
- 任意 Git/CL/comment/webhook 事件可以创建一个 `agent_run` stub。
- 每个 run 有完整状态机：queued、running、waiting_approval、succeeded、failed、
  cancelled、timed_out。
- 所有 mutating 工具调用都能写入 `agent_steps` 和 audit log。

### P1：通用 Trigger Engine

在 `build_trigger` 之外新增面向 Agent 的触发层，复用但不污染构建触发模型。

新增能力：

- 事件类型：repo push、CL created/updated、review comment、issue comment、build failed、
  artifact uploaded、chat mention、webhook received、cron schedule。
- 过滤语言：优先 JSONLogic；字段包括 repo、branch、path、actor、event_type、labels、
  build_status、comment body。
- loop guard：识别 bot/service account、自身产生的评论、重复 webhook delivery。
- trigger preview：给定事件 payload，返回会命中的 trigger 和原因。

验收标准：

- `Webhook` 和 `Schedule` 不再只是 build trigger 的保留枚举，而能真正触发 Agent run。
- CL 评论中的 slash command 可触发指定 Expert。
- 构建失败事件可触发诊断 Expert，但不会被自身评论再次触发。

### P1：Tool/Capability Registry

把 monoengine 内部模块包装为 Agent tool，并纳入权限、审计、审批。

首批内部工具：

- `repo.read_file`、`repo.search`、`repo.diff`、`repo.create_branch`
- `cl.comment`、`cl.update_description`、`cl.request_review`
- `code_review.create_thread`、`code_review.reply`、`code_review.resolve`
- `merge_queue.add`、`merge_queue.retry`、`merge_queue.status`
- `build.trigger`、`build.status`
- `artifact.put`、`artifact.get`、`artifact.link_to_session`
- `notification.send`、`chat.send_message`
- `webhook.dispatch`

权限规则：

- 读工具默认只需 repo read 权限。
- 写工具需要 repo/CL/issue 对应权限。
- 外部副作用工具默认需要审批或 Expert 级白名单。
- Secret 访问必须通过 capability 声明和 Vault path scope 绑定。

验收标准：

- 每个工具有 schema、权限检查、审计字段和错误分类。
- Agent run 中的工具调用能被重放/查看，但不会泄露 secret。

### P2：Context Engine MVP

先实现对 monoengine 自有对象的上下文索引，再接入外部文档和 MCP。

索引对象：

- Git tree 文件、语言符号、README、配置文件。
- commit、CL diff、review thread、issue、note/conversation。
- build trigger payload、build log、artifact manifest。
- docs/refactoring、docs 目录计划和 ADR。
- chat channel 中被标记为知识沉淀的消息。

检索能力：

- `context.search(repo, query, filters)`：混合全文和语义检索。
- `context.related(entity)`：围绕 CL/issue/build/session 找相关文件、历史、讨论。
- `context.pack(run_id, budget)`：为模型调用打包上下文，并保存引用。
- 权限过滤和引用输出必须是强约束。

实现建议：

- 第一阶段使用 Postgres full-text / SQLite FTS 或 Tantivy 类本地索引，避免过早引入
  向量数据库。
- 第二阶段增加 embedding provider 抽象和可选向量列/外部向量服务。
- 所有上下文结果都必须包含 `source_type`、`source_id`、`path`、`commit`、`line_range`
  或等价引用。

验收标准：

- 给定 CL，能自动召回 touched files、相关旧 review、相关 issue、失败构建和计划文档。
- Agent 生成评论时能记录其引用的上下文来源。

### P2：Human-in-the-loop 与 Chat/Notification 集成

将现有 Chat 和 Notification 作为 Agent 操作界面，而不是新建一套交互层。

新增能力：

- `@agent` / slash command：在 CL、issue、chat 中触发 Expert。
- 审批消息：敏感命令、外部发布、merge queue、secret 使用前发起确认。
- 运行状态：queued/running/waiting/failed/succeeded 通过 in-app/email/webhook/未来 Slack
  通知。
- Chat thread 与 `agent_session` 互相关联，用户可以继续追问、恢复或取消 run。

验收标准：

- 用户可在 chat 中发起 Agent run，并看到状态和摘要。
- Agent 需要审批时会暂停，用户确认后继续。
- 失败 run 会给出可复现日志和下一步建议。

### P3：AI Code Review 与 PR/CL 专家

在 Agent Runtime 稳定后，把 Augment 类 code review 作为第一批产品化 Expert。

新增 Expert：

- `DeepCodeReviewExpert`：高信号 bug/security/correctness/cross-system review。
- `PairReviewExpert`：跟随作者迭代，回答评论、解释修改、建议补丁。
- `RiskAnalysisExpert`：输出风险等级、影响面、测试建议、回滚建议。
- `TestExpert`：根据 diff 生成/运行测试，上传日志和覆盖证据。
- `PRAuthorExpert`：生成 CL 描述、变更摘要、测试计划、迁移说明。

新增配置：

- 仓库级 review guideline。
- 路径/语言级规则。
- 自动/手动/禁用三种触发模式。
- 最小风险阈值和最大评论数。
- 反馈按钮：有用、误报、已修复、不相关。

验收标准：

- 新建或更新 CL 后可自动创建 AI review run。
- AI review 能写入现有 code_review thread，而不是旁路评论系统。
- 每条 AI 评论都能追溯 run、模型、上下文引用和 guideline。
- Analytics 能统计接受率、误报率、修复率和耗时。

### P3：MCP Client/Server

新增 MCP 双向能力，使 monoengine 既能调用外部工具，也能被外部 Agent 使用。

MCP client：

- 组织/仓库/用户级 MCP server 配置。
- 工具 schema 拉取、权限映射、secret 引用、超时和重试。
- Agent run 中记录 MCP tool 调用、输入输出摘要和错误。

MCP server：

- 暴露 repo search、context search、CL/issue/build/artifact 查询。
- 写操作默认只暴露给已授权 service account。
- 支持只读模式，便于 Codex、Claude、Gemini 等外部工具接入 monoengine 上下文。

验收标准：

- Agent 可调用一个外部 MCP 工具，并在 run log 中审计。
- 外部 MCP client 可查询 monoengine repo/CL/context，但不会越权。

### P4：远程沙箱与执行隔离

初期可使用本机 worker 验证领域模型，但对标 Augment 需要远程或隔离执行环境。

新增能力：

- per-run checkout/worktree。
- 容器或 VM worker，支持镜像、资源限制、网络策略。
- secret 启动注入，退出清理。
- 命令输出实时流式记录，secret redaction。
- checkpoint/restore：用户可回到某个 step 前的状态。
- 并发运行：同一 repo 多个 run 隔离，不互相污染。

验收标准：

- 同一 CL 可并行运行两个 Agent 方案，产出不同 branch/artifact。
- 失败 run 不会污染宿主 repo 和长期 secret。

### P4：组织知识、记忆与分析

新增能力：

- expert memory：团队偏好、常见架构约束、已接受/拒绝建议。
- repo memory：模块边界、测试策略、部署风险、历史事故。
- user memory：个人偏好和审批习惯，仅在授权范围内使用。
- analytics：运行量、成功率、平均耗时、token/成本、工具失败、人工审批耗时。
- 管理界面：Expert 管理、trigger 管理、secret scope、run 搜索、反馈报表。

验收标准：

- AI review 可以读取仓库级 guideline 和历史反馈。
- 管理员可看到各 Expert 的质量和成本趋势。

## 建议落地顺序

### 阶段 0：冻结边界与 schema 设计

目标：

- 新建 Agent refactoring 计划或 ADR，明确 Agent 与 build trigger、notification、chat、
  artifact、bot、Vault、policy 的边界。
- 设计 migration，不接模型，不引入 LLM 依赖。

完成条件：

- 领域模型通过 review。
- 可以从源码解释每个现有模块如何被复用，而不是重建平行系统。

### 阶段 1：事件到 run 的最小闭环

目标：

- 实现 `agent_session`、`agent_run`、`agent_step`。
- 实现 trigger engine 的最小版本。
- CL comment 或 webhook 可以创建 run stub。

完成条件：

- 无模型调用也能完整记录一次自动化运行。
- 用户能在 API/CLI 查询 run 状态、步骤和失败原因。

### 阶段 2：工具注册与只读 Agent

目标：

- 包装 repo/search/context/code_review 查询工具。
- 支持只读 Agent 生成分析报告，但不改代码、不发评论。

完成条件：

- Agent 能围绕一个 CL 读取 diff、相关文件、历史评论和文档，输出结构化报告。
- 所有工具调用可审计。

### 阶段 3：受控写操作与 AI Review

目标：

- 接入模型 provider 抽象。
- 允许 Agent 在审批或白名单下创建 code_review thread。
- 增加 review guideline 和反馈闭环。

完成条件：

- 可手动触发 AI review。
- 生成的评论进入现有 code_review 系统。
- 误报反馈可回写 run/guideline 数据。

### 阶段 4：CI/CLI/MCP 自动化

目标：

- 增加 Auggie 类 CLI。
- 增加 MCP client/server。
- 增加 build failure 和 scheduled trigger。

完成条件：

- CI 中可运行 `mono agent run --print` 类命令。
- 外部工具可通过 MCP 查询 monoengine 上下文。
- 构建失败可触发诊断 Expert 并输出 artifact。

### 阶段 5：远程执行与企业治理

目标：

- 增加 container/VM worker。
- 增加 service account、secret scope、预算、analytics、管理界面。

完成条件：

- 多个 Agent run 可并发隔离执行。
- 管理员可审计谁触发了什么、用了哪些 secret、写了哪些评论、产出了哪些 artifact。

## 不建议优先做的事

- 不建议先把 OpenAI/Anthropic SDK 直接塞进业务 router。模型调用应在 Agent Runtime
  和 provider 抽象之后进入。
- 不建议把 AI review 写成独立旁路评论系统。应复用现有 `code_review` thread/comment
  模型，否则审计、权限和 UI 会分裂。
- 不建议让 build trigger 直接承担所有自动化事件。构建触发只是 Agent trigger 的一个
  事件源，应保持领域边界。
- 不建议在 Context Engine 未有权限过滤前接入组织级外部知识，否则容易越权召回。
- 不建议把 secret 作为普通环境变量长期保存。应使用 Vault 引用、per-run 注入和日志脱敏。

## 最小可交付功能清单

若只做第一版对标 Augment 的 monoengine Agent MVP，建议范围限定为：

1. `agent_session` / `agent_run` / `agent_step` migration 与 storage。
2. `agent_router`：create/list/get/cancel run。
3. `agent_trigger`：CL comment slash command、manual API、webhook received。
4. `agent_tool`：只读 repo diff/search、CL metadata、code_review list。
5. `context_pack`：基于 touched files + docs + review history 的规则召回。
6. `AIReviewExpert`：手动触发，只写 code_review thread，带 guideline 和上下文引用。
7. `notification` 集成：run success/failure/waiting_approval 通知。
8. `artifact` 集成：保存 review report/test log。
9. `audit` 集成：记录工具调用、模型版本、触发来源、服务身份。

这组能力完成后，monoengine 才具备从“代码托管协作平台”升级到“Agent 驱动的
软件工程平台”的最小闭环。

## 资料来源

外部公开资料访问日期均为 2026-06-21：

- Augment Code 官网：https://www.augmentcode.com/
- Augment 文档 Introduction：https://docs.augmentcode.com/introduction
- Augment Agent 文档：https://docs.augmentcode.com/using-augment/agent
- Augment Cosmos Getting Started：https://docs.augmentcode.com/using-cosmos/getting-started
- Augment Context Engine Overview：https://docs.augmentcode.com/context-engine/overview
- Augment Context Engine MCP：https://docs.augmentcode.com/context-engine/mcp
- Auggie CLI 文档：https://docs.augmentcode.com/auggie/overview
- Auggie Automation 文档：https://docs.augmentcode.com/auggie/guides/automation
- Code Review 文档：https://docs.augmentcode.com/code-reviews
- Enterprise Code Review 文档：https://docs.augmentcode.com/enterprise/code-reviews/setup-github-code-review
- MCP Servers 文档：https://docs.augmentcode.com/setup-augment/mcp
- Cosmos GitHub 配置：https://docs.augmentcode.com/using-cosmos/configuring-github
- Cosmos Secrets：https://docs.augmentcode.com/using-cosmos/secrets
- Cosmos Artifacts：https://docs.augmentcode.com/using-cosmos/artifacts
- Remote Agents 发布说明：https://www.augmentcode.com/blog/remote-agents-launch
- Augment Changelog：https://www.augmentcode.com/changelog
