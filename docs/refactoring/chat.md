# Chat 模块迁移与改进计划

本文档记录 `monoengine` 中 Chat 引擎的现状分析、迁移规范、分阶段落地计划，用于把源 Rails 后端中的聊天能力完整迁入并完善。迁移后的代码、表、API、类型、配置和网关命名必须使用 `chat`、`channel`、`message` 等业务语义，不再使用源项目名称作为前缀或命名空间。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **与其他模块的依赖**：Chat 模块的通知功能（message 提及/回复通知）将依赖 **`notification.md`** 的多渠道通知系统。Mail 模块完成后，可支持聊天通知的邮件投递。当前集成测试计划参见 **`integration.md`**。

## 事实校准（2026-06-23）

> 本文档中的代码引用已对照当前 `src/` 重新核对。早期“仅框架/未实现”的描述已经过期，当前 chat 模块已有可用的 CRUD 主干：

1. **Chat 模块边界和服务层已落地**。`src/chat/mod.rs`、`src/chat/domain.rs`、`src/chat/engine.rs`、`src/chat/service/{shared,channel_chat}.rs` 存在，`SharedFoundations` 与 `ChannelChat` 两个 capability 已有 storage/service 主路径。
2. **SeaORM 实体与迁移已落地**。`attachments`、`custom_reactions`、`open_graph_links`、`channels`、`channel_memberships`、`channel_membership_updates`、`messages`、`message_notifications` 等实体/迁移存在；`reactions` 复用并扩展既有表。
3. **HTTP Router 已接入**。`src/api/router/chat_router.rs` 已在 `src/api/api_router.rs` merge，提供 channel CRUD、message CRUD、reaction、attachment presign/confirm、read/unread 端点并带 OpenAPI 标注。
4. **权限主干已实现并在本轮收紧**。channel list/detail/message list/send/reaction/attachment 等路径会校验 membership；2026-06-23 新增 message edit/delete 的 channel path 校验和当前 membership 校验，避免只凭 sender ownership 跨 channel path 或被移除成员继续写旧消息；同日 `chat_router` 的 message/custom-reaction 映射读取已下沉到 storage helper，减少 handler 直接 SeaORM 查询。
5. **仍未完成**：外部数据迁移工具的真实源库导入/校验、WebSocket/Pusher 兼容网关、完整真实 HTTP 黑盒矩阵和外部通知投递。message notification 内部状态已完成首批 reply 与 `@username` mention 写入；更完整 rich-text mention 语义仍属于后续。附件 presign/confirm 已有首批 file name/type/size/object-key 校验；后续仍需按产品策略补更完整 MIME allowlist 与存储对象归属校验。实时事件已有 `NoopChatEvents` 默认实现和 `InMemoryChatEvents` 进程内 broadcast hub，可供测试与后续网关消费。mark unread 已按验收标准把 `last_read_at` 调到 latest message 之前（2026-06-23 补齐）。

## 当前实现状态速览表

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
|-----------|--------|-------------|
| Chat 模块入口 | 已实现主干 | `src/chat/` 下已有 domain/engine/service；仍需继续清理文档中的旧 slice 叙述。 |
| Shared Foundations（附件、表情） | 部分实现 | attachment/reaction/custom reaction/open graph 的实体、迁移、storage 与 service 主路径已落地；附件 presign/confirm 已补 file name/type/size/object-key 首批校验；custom_reactions 已补 `lower(name)` 唯一索引和应用层 lowercase；reactions 已改为 `WHERE discarded_at IS NULL` 部分唯一索引；attachment 已补 `discarded_at` 软删除（满足硬约束 #4）；仍缺产品级 MIME allowlist、对象归属复核和链接预览抓取。 |
| Channel Chat（频道、消息） | 部分实现 | channel/message/membership 实体、迁移、storage、service 已落地；create/send/edit/delete/read/unread/member service 主路径可用；reply 与 `@username` mention message notification 内部状态已写入，rich-text mention 解析和外部投递仍未实现；channels/channel_memberships/channel_membership_updates/messages/message_notifications 的完整索引集和 `message_notifications` 唯一约束已补齐。 |
| HTTP API | 部分实现 | `chat_router` 已挂载，DTO/OpenAPI 标注存在；仍缺真实 HTTP 黑盒矩阵、成员管理 HTTP 端点是否暴露的产品决策，以及更完整错误码兼容性。 |
| 实时事件 | 进程内 broadcaster 已实现 | `ChatEvents`/`NoopChatEvents` 已定义并由 service 调用；`InMemoryChatEvents` 已提供 tokio broadcast 订阅能力并覆盖 service mutation 事件。WebSocket/Pusher 兼容网关仍未实现。 |
| 数据迁移工具 | 初步 CLI | `src/commands/chat_migrate.rs` 存在；已补空库守卫（拒绝在已有 channel 数据的库上运行）和验证报告（对比导入计数与 DB 实际计数）；仍需真实源库脱敏 fixture 和更完整端到端 exec 测试。 |
| 权限控制 | 首批实现并加固 | 多数 channel/message 路径已校验 membership；2026-06-23 已补 message edit/delete 的 channel path + current membership guard，并移除 `chat_router` response mapping 中对 message/custom-reaction 的直接 SeaORM 查询；同日 `delete_reaction` 已补 current membership guard，被移除成员不能再删除自己的旧 reaction。仍需持续把其他 handler 内直接 SeaORM 查询迁回 storage/service。 |
| 集成测试 | 部分实现 | service/router 生命周期测试存在；router 测试在 Redis 不可用时会 skip，需要补更稳定的无 Redis 黑盒覆盖。 |

## 硬约束与不可违反的原则

1. **命名纯净**：生产代码、schema、API、配置、日志中不得出现源项目名称。允许的例外仅限：历史迁移脚本注释、git commit message、一次性工具输出。

2. **用户身份统一**：所有用户引用转换为 `username`，不迁移源 `users` 表。不引入组织维度的多租户抽象。

3. **权限强制**：storage 层所有 read/write 操作必须 JOIN `channel_memberships` 验证当前用户的成员身份。handler 层禁止直接调用 SeaORM。

4. **Soft Delete 一致性**：所有可删除资源（channel、message、attachment 等）保留 `discarded_at`，storage 默认过滤 `discarded_at IS NULL`。

5. **API 不泄露内部 ID**：HTTP response 只暴露 `public_id` 和 `username`，不暴露数据库自增 ID。

6. **范围边界**：仅迁移聊天频道 CRUD、消息、附件、表情、reaction、成员管理。不包括 Auth、Org、Project、Notes、Posts、Notifications、Integrations、Calls、Data export 等。

## 现状 vs 目标对比

| 维度 | 当前状态 | 目标状态 | 关键差距 |
|-----|--------|--------|--------|
| 模块组织 | 框架占位 | 功能完整、可测试、可部署 | 需实装 6 个切片 |
| 数据存储 | MySQL/PlanetScale | PostgreSQL/monoengine DB | 需迁移 9 张表与数据 |
| 代码结构 | 仅 engine.rs | callisto entities + storage + service + API | 需按分层补齐 |
| API | 不存在 | /api/v1/chat/* 12+ 端点 + OpenAPI | 需新增 HTTP router 与 handler |
| 认证 | 无 | HTTP Bearer/Basic + SSH key | 需 protocol auth context |
| 权限 | 无 | 成员身份强制检查 | 需 storage 层 JOIN 检查 |
| 测试 | 无 | 单元测试 + 集成测试 + 数据迁移测试 | 需补齐测试矩阵 |

## 前置依赖矩阵（2026-06-14）

本文档与其他改进计划的依赖关系：

| Chat 的工作 | 对其他模块的依赖 | 依赖类型 | 关键同步点 |
|-----------|-------------|--------|---------|
| **Slice 1-2：Schema + Storage** | general.md | 框架 | 必须遵守结构规范和评审标准 |
| **Slice 3：Service** | mail.md（可选） | 后置 | 通知功能与 mail 的 password_ref 协同 |
| **Slice 4-5：API + Upload** | config.md（可选） | 后置 | 若需要配置凭据，使用 config 的 SecretRef |
| **Slice 6：数据迁移** | notification.md（可选） | 独立 | 迁移完成后可支持聊天通知 |

**注**：Chat 的核心 CRUD 功能与其他模块独立，可先完成 Slice 0-5。Slice 6 数据迁移可与其他工作并行。

## 风险与约束

- **迁移数据一致性**：9 张表跨 MySQL 到 PostgreSQL，需要完整的用户映射和冲突检测。跳过 integration/call/post 派生消息会导致聊天断层。

- **权限隔离**：storage 层如果不强制 membership 检查，会产生数据泄露风险。当前框架设计明确了这一点，但实施时必须在每个查询点验证。

- **Soft Delete 查询复杂性**：大量查询需要默认过滤 `discarded_at IS NULL`，如果不在 storage 层统一处理，会导致遗漏和不一致。

- **实时事件复杂性**：当前已有 no-op 默认实现和进程内 broadcast hub，但后续接入 WebSocket/Pusher 时仍需要网关层适配。事件接口应继续保持稳定。

- **API 认证一致性**：HTTP 和 SSH 认证上下文需与其他模块（如 vault、config 的 protocol auth）保持一致。

## 多维评估表

| 维度 | 评估结论 |
|-----|--------|
| **合理性** | **高（8.5/10）**。迁移范围清晰，功能边界明确，架构分层合理。关键约束（权限强制、命名纯净）直指常见陷阱。 |
| **可行性** | **高（8/10）**。现有框架完整，SeaORM 模式已建立，storage + service + API 的分层路径清晰。数据迁移工具是最大变量，需提前设计和测试。 |
| **完整性** | **中（7/10）**。6 个切片覆盖功能主干，但测试策略和监控策略相对薄弱。迁移完成后的灰度切、回滚路径需要补齐。 |
| **安全性** | **中（7/10）**。权限模型和 soft delete 设计正确，但多个风险点（跨库迁移、事件时序、LFS hybrid）需要在各切片中逐一验证。 |
| **可扩展性** | **中（7.5/10）**。当前架构支持后续的多渠道通知、webhook、搜索等扩展。但切片划分较松散，后续改造成本可能较高。 |

## 小结

Chat 模块是一个从零迁移的大型功能模块，涉及 9 张表、12+ API 端点、完整的权限模型和实时事件机制。当前 schema、storage、service 和绿地 HTTP API 主干已经落地；下一步重点不再是“从零开始”，而是补齐权限一致性、真实 HTTP/迁移测试、message notification、实时事件和数据迁移闭环。

## 预期收益

- **完整聊天能力**：支持频道、消息、附件、表情、reaction、成员管理的端到端用户体验
- **数据一致性**：通过统一的 soft delete 和权限检查，确保数据隐私和完整性
- **易于扩展**：清晰的分层架构（entity → storage → service → API）为后续多渠道通知、搜索、webhook 奠定基础
- **可维护性**：明确的命名规范和 review checklist 降低后续维护成本
- **可观测性**：统一的权限模型和事件机制便于审计和监控

## 命名决策

本轮迁移采用产品语义命名：

- 表名不加项目来源前缀，直接使用 `channels`、`channel_memberships`、`messages`、`attachments`、`reactions` 等名称。
- Rust 模块使用 `src/chat/`。
- Rust 类型使用 `ChatEngine`、`ChatCapability`、`ChatEntityKind`。
- HTTP API 使用 `/api/v1/chat/...` 或 `/api/v1/channels/...`。
- 网关、事件、trait 使用 `chat` 或 `channel` 命名，例如 `ChatEvents`、`channel-message-created`。
- 迁移后的目标代码中不得出现源项目名称。

允许出现源项目名称的地方仅限：历史说明、提交记录、一次性导入脚本的注释或外部源路径说明。生产运行时代码、数据库 schema、HTTP API、OpenAPI、配置项、指标和日志字段都不得使用源项目名称。

## 目标范围

本次只迁移一个产品面：**聊天频道**。

- 每条 legacy `message_thread` 迁移为一个 `channel`。
- 支持频道列表、频道详情、创建/更新/删除频道、消息列表、发送/编辑/删除消息、回复、附件、消息表情反应、成员变更、已读/未读状态。
- 用户身份直接复用 `monoengine` 现有用户模型，聊天表使用 `username` 作为稳定引用。

## 非目标范围

以下功能不进入本轮迁移：

- Auth：session、OAuth2、desktop/Figma sign-in、OTP、recovery code、用户偏好。
- Organization：组织、邀请、加入申请、角色、SSO、计费、feature flag。
- Project：项目空间、项目成员、项目 pin/bookmark/favorite/display preference。
- Notes：协作文档、Yjs、文档权限、公开分享、timeline event。
- Posts：feed、草稿/发布、评论、poll、tag、post views、feedback request、TLDR/resolution。
- Notifications：通知中心、归档/已读、email digest、web-push、Slack 投递、scheduled notification。
- Integrations：Slack、Linear、Figma、HMS、Cal.com、Zapier、GitHub、webhook。
- Calls：call room、peer、recording、transcription、summary。
- Data export：导出任务、审计、product log。
- Styled text service：`markdown_to_html` 和 `html_to_slack` 独立服务。

## 当前代码状态

`monoengine` 已经有聊天引擎初始边界：

- `src/chat/mod.rs`：Chat 引擎模块入口。
- `src/chat/domain.rs`：`ChatCapability`、`ChatEntityKind`、`ChatEntityRef`、`ChatMigrationSlice`。
- `src/chat/engine.rs`：`ChatEngine` 门面与迁移切片注册表。
- `MIGRATION_SLICES` 当前只包含 `SharedFoundations` 和 `ChannelChat`。

## 源系统事实

核心 Rails 组件：

- `api/app/models/message_thread.rb`
- `api/app/models/message.rb`
- `api/app/models/message_thread_membership.rb`
- `api/app/models/message_thread_membership_update.rb`
- `api/app/models/message_notification.rb`
- `api/app/models/attachment.rb`
- `api/app/models/reaction.rb`
- `api/app/models/custom_reaction.rb`
- `api/app/models/open_graph_link.rb`
- `api/app/controllers/api/v1/message_threads_controller.rb`
- `api/app/controllers/api/v1/message_threads/messages_controller.rb`
- `api/app/controllers/api/v1/messages/reactions_controller.rb`
- `api/app/controllers/api/v1/reactions_controller.rb`

源数据库事实：

- 源 schema 共 94 张表。
- 本轮迁移范围内 9 张表。
- 其余 85 张表不迁移。

源 Rails 行为要点：

- `MessageThread#index` 查询当前成员参与的非 project threads，按 `last_message_at desc, id desc` 排序。
- `MessageThread#create` 创建 thread 和 memberships，可选创建首条 message。
- `MessageThread#update` 只更新 `title` 和 `image_path`。
- `MessageThread#destroy` 在 Rails 中 hard destroy；Rust 端统一改成 soft delete，写入 `discarded_at`。
- `Messages#index` 从 thread 下拉消息，按 `id desc` 分页。
- `Messages#create` 写 message、附件、reply_to，维护 thread 的 `latest_message_id` 和 `last_message_at`。
- `Messages#update` 更新内容并广播 invalidate。
- `Messages#destroy` soft delete message，如果删除的是 latest message，需要回算 channel latest message。
- `Messages::Reactions#create` 在 message 上创建 reaction，并触发 message invalidate。

## 目标架构

模块边界：

- SeaORM 实体放在 `src/callisto/`，文件名与表名一致，例如 `channel.rs`、`message.rs`、`attachment.rs`。如果与现有实体冲突，则优先使用更明确的业务名，例如 `chat_message.rs`，但表名仍不加来源前缀。
- Storage 放在 `src/jupiter/storage/chat_storage.rs` 或按领域拆分为 `channel_storage.rs`、`message_storage.rs`、`attachment_storage.rs`。
- 领域服务放在 `src/chat/service/`。
- HTTP 路由放在 `src/api/router/chat_router.rs` 或 `channel_router.rs`。
- DTO 放在 `src/contract/api/chat/`，除非现有 `contract::api` 模式要求平铺。
- 迁移放在 `src/jupiter/migration/`，文件名使用 `create_chat_tables` 或 `create_channel_tables` 这类语义名称。

调用方向：

```text
HTTP handler -> chat service -> typed storage -> callisto entity
                      |
                      +-> event adapter
```

禁止事项：

- Handler 中禁止直接写 SeaORM 查询。
- 目标表名、目标模块、目标 API 不得使用源项目名称前缀。
- 不要保留指向已砍功能的外键列。
- 不要引入 Rails 兼容的多组织、多项目抽象。

## 架构决策表

| 编号 | 决策 | 状态 | 执行规格 |
|------|------|------|----------|
| D1 | 授权 | 已接受 | 行级访问由 storage 查询强制 `JOIN channel_memberships`，端点级粗权限复用 monoengine 现有策略体系。 |
| D2 | 异步任务 | 暂缓 | 第一版不建 chat jobs 表。链接预览和提及解析先同步执行或留空字段；确有异步需求时再按独立切片设计。 |
| D3 | 实时分发 | 已接受 | 第一版实现内部事件广播 trait；是否兼容 Pusher 协议作为独立切片，不能阻塞 CRUD。 |
| D4 | 附件上传 | 已接受 | 使用 object storage 预签名直传；确认接口写入 `attachments`。 |
| D5 | public_id | 已接受 | 生成 12 字符 public ID，保持唯一索引；先保证稳定格式，不承诺复现 Rails 随机序列。 |
| D6 | soft delete | 已接受 | 范围内可删除资源均保留 `discarded_at`，storage 默认过滤。 |
| D7 | 搜索 | 不适用 | Search 不迁移。 |
| D8 | 用户身份 | 已接受 | 所有用户引用转换为 `username`。不迁移源 `users` 表。 |
| D9 | HTTP namespace | 已接受 | 新端点使用 `/api/v1/chat/...`；Rails 兼容路径只在明确客户端需要时加 feature gate。 |
| D10 | 数据迁移 | 已接受 | 先全量导入范围内 9 表并做字段转换，再灰度切读写。 |

## 目标 Capability

### SharedFoundations

职责：附件、表情、自定义表情、链接预览。

目标文件：

- `src/callisto/attachment.rs`
- `src/callisto/reaction.rs`
- `src/callisto/custom_reaction.rs`
- `src/callisto/open_graph_link.rs`
- `src/jupiter/storage/attachment_storage.rs`
- `src/jupiter/storage/reaction_storage.rs`
- `src/chat/service/shared.rs`

### ChannelChat

职责：聊天频道、成员、成员变更记录、消息、聊天内 message notification 状态。

目标文件：

- `src/callisto/channel.rs`
- `src/callisto/channel_membership.rs`
- `src/callisto/channel_membership_update.rs`
- `src/callisto/message.rs`
- `src/callisto/message_notification.rs`
- `src/jupiter/storage/channel_storage.rs`
- `src/jupiter/storage/message_storage.rs`
- `src/chat/service/channel_chat.rs`

## 目标 Schema

所有表默认字段规则：

- 主键使用 monoengine 现有 SeaORM 迁移惯例。
- 保留 `public_id varchar(12)` 并加唯一索引，除非源表没有 public_id。
- 时间字段使用项目现有 timestamp 类型惯例。
- 可删除表保留 `discarded_at`。
- 用户引用字段使用 `username`，不使用 `user_id`、`organization_membership_id`、`owner_id`、`sender_id`、`actor_id`。

### `attachments`

保留字段：

| 字段 | 说明 |
|------|------|
| `public_id` | 客户端可见 ID，唯一。 |
| `file_path` | object storage key 或外链 URL。 |
| `file_type` | MIME 或 link type。 |
| `subject_type` | 第一版只允许 `Message`。 |
| `subject_id` | 指向 `messages.id`。 |
| `preview_file_path` | 可选预览资源。 |
| `width` / `height` / `duration` | 媒体元数据。 |
| `position` | 同一消息内附件排序。 |
| `name` / `size` | 文件展示信息。 |
| `gallery_id` | 保留，兼容多附件 gallery。 |
| `created_at` / `updated_at` | 时间戳。 |

删除字段：

- `figma_file_id`
- `remote_figma_node_id`
- `remote_figma_node_type`
- `remote_figma_node_name`
- `figma_share_url`
- `transcription_job_id`
- `transcription_job_status`
- `transcription_vtt`
- `comments_count`
- `imgix_video_file_path`
- `no_video_track`

索引：

- unique `public_id`
- `(subject_type, subject_id)`
- `(subject_type, subject_id, position)` 可选，用于附件排序。

### `reactions`

保留字段：

| 字段 | 说明 |
|------|------|
| `public_id` | 客户端可见 ID，唯一。 |
| `content` | unicode emoji。 |
| `subject_type` | 第一版只允许 `Message`。 |
| `subject_id` | 指向 `messages.id`。 |
| `username` | 反应创建者。 |
| `custom_reaction_id` | 可选自定义表情。 |
| `discarded_at` | soft delete。 |
| `created_at` / `updated_at` | 时间戳。 |

索引：

- unique `public_id`
- `(subject_type, subject_id)`
- unique `(subject_type, subject_id, username, content, custom_reaction_id, discarded_at)`，迁移时确认 Postgres 对 nullable unique 的语义是否满足需求；不满足则使用 partial unique index。
  - **已核实（2026-06-24）**：当前 `idx-reactions-unique-active` 是 `WHERE discarded_at IS NULL` 的 partial unique index，但因其包含 nullable 列（`content` / `custom_reaction_id`），Postgres 默认把 NULL 视为互不相同，实际不会对标准 emoji（`custom_reaction_id` NULL）或 custom reaction（`content` NULL）去重。若要真正去重，需改用 `NULLS NOT DISTINCT`（Postgres 15+）重建索引——属后续 chat 专项，暂不在本次范围。

### `custom_reactions`

保留字段：

| 字段 | 说明 |
|------|------|
| `public_id` | 客户端可见 ID，唯一。 |
| `name` | 表情名称。 |
| `file_path` | 图片资源。 |
| `file_type` | MIME。 |
| `username` | 创建者。 |
| `pack` | 源表保留字段。 |
| `created_at` / `updated_at` | 时间戳。 |

删除字段：

- `organization_id`
- `organization_membership_id`

索引：

- unique `public_id`
- unique `lower(name)` 或应用层统一 lowercase 后 unique `name`。

### `open_graph_links`

保留字段：

| 字段 | 说明 |
|------|------|
| `url` | 链接 URL。 |
| `title` | 展示标题。 |
| `image_path` | 可选图片。 |
| `favicon_path` | 可选 favicon。 |
| `created_at` / `updated_at` | 时间戳。 |

索引：

- unique `url`，源表没有索引，Rust 端应加，避免重复抓取。

### `channels`

保留字段：

| 字段 | 说明 |
|------|------|
| `public_id` | 客户端可见 ID，唯一。 |
| `title` | 可空频道标题。 |
| `last_message_at` | 排序用。 |
| `latest_message_id` | 可空，指向最新未删除消息。 |
| `members_count` | 冗余计数。 |
| `image_path` | 频道图。 |
| `group` | 是否多人/群组。 |
| `notification_forced_at` | 保留，若第一版不用则不暴露 API。 |
| `owner_username` | 创建者。 |
| `discarded_at` | soft delete。 |
| `created_at` / `updated_at` | 时间戳。 |

索引：

- unique `public_id`
- `last_message_at`
- `latest_message_id`
- `discarded_at`
- `owner_username`

### `channel_memberships`

保留字段：

| 字段 | 说明 |
|------|------|
| `channel_id` | channel FK。 |
| `username` | 成员身份。 |
| `last_read_at` | 已读位置。 |
| `manually_marked_unread_at` | 手动未读。 |
| `notification_level` | 只作为聊天内偏好保留，不触发外部通知。 |
| `created_at` / `updated_at` | 时间戳。 |

索引：

- unique `(channel_id, username)`
- `username`
- `last_read_at`
- `manually_marked_unread_at`

### `channel_membership_updates`

保留字段：

| 字段 | 说明 |
|------|------|
| `channel_id` | channel FK。 |
| `actor_username` | 操作者。 |
| `added_usernames` | JSON array。 |
| `removed_usernames` | JSON array。 |
| `discarded_at` | soft delete。 |
| `created_at` / `updated_at` | 时间戳。 |

索引：

- `channel_id`
- `actor_username`
- `discarded_at`

### `messages`

保留字段：

| 字段 | 说明 |
|------|------|
| `channel_id` | channel FK。 |
| `sender_username` | 可空，系统消息时为空。 |
| `content` | HTML/rich text 内容；允许空字符串但服务层要求内容或附件至少一个存在。 |
| `public_id` | 客户端可见 ID，唯一。 |
| `reply_to_id` | 可空，引用同表。 |
| `unfurled_link` | 可选，第一版可保留但不自动生成。 |
| `discarded_at` | soft delete。 |
| `created_at` / `updated_at` | 时间戳。 |

索引：

- unique `public_id`
- `channel_id`
- `(channel_id, id)` 用于分页。
- `sender_username`
- `reply_to_id`
- `discarded_at`

### `message_notifications`

保留字段：

| 字段 | 说明 |
|------|------|
| `channel_membership_id` | channel membership FK。 |
| `message_id` | message FK。 |
| `created_at` / `updated_at` | 时间戳。 |

用途限制：

- 只表示聊天内“这条消息对这个成员有提醒语义”，例如 mention 或 reply。
- 不得驱动 email、web-push、Slack 或系统通知中心。

索引：

- unique `(channel_membership_id, message_id)`
- `message_id`

## HTTP API

第一版只提供绿地 API：`/api/v1/chat/...`。

| 方法 | 路径 | 语义 |
|------|------|------|
| `GET` | `/api/v1/chat/channels` | 当前用户可见 channel 列表。 |
| `POST` | `/api/v1/chat/channels` | 创建 channel，可选发送首条消息。 |
| `GET` | `/api/v1/chat/channels/{channel_id}` | channel 详情。 |
| `PATCH` | `/api/v1/chat/channels/{channel_id}` | 更新 `title` / `image_path`。 |
| `DELETE` | `/api/v1/chat/channels/{channel_id}` | soft delete channel。 |
| `GET` | `/api/v1/chat/channels/{channel_id}/messages` | 消息分页。 |
| `POST` | `/api/v1/chat/channels/{channel_id}/messages` | 发送消息。 |
| `PATCH` | `/api/v1/chat/channels/{channel_id}/messages/{message_id}` | 编辑消息。 |
| `DELETE` | `/api/v1/chat/channels/{channel_id}/messages/{message_id}` | soft delete 消息。 |
| `POST` | `/api/v1/chat/messages/{message_id}/reactions` | 创建 reaction。 |
| `DELETE` | `/api/v1/chat/reactions/{reaction_id}` | soft delete reaction。 |
| `POST` | `/api/v1/chat/attachments/presign` | 获取上传 URL。 |
| `POST` | `/api/v1/chat/attachments` | 注册已上传附件。 |

请求身份：

- Handler 必须从 monoengine 现有 auth/session 机制拿到当前 `username`。
- 所有 channel/message 读取必须在 storage 层校验 membership。
- 发送消息时，如果当前用户不是 channel member，返回 404，减少资源枚举，并写进 API 测试。

## 服务行为规格

### 创建 channel

输入：`title?`、`image_path?`、`member_usernames[]`、`group?`、`initial_message?`、`attachments[]?`。

行为：

- 自动把创建者加入 `member_usernames`。
- 去重成员列表。
- 创建 `channels`。
- 为每个成员创建 `channel_memberships`。
- 写一条 membership update，`actor_username` 为创建者。
- 如果有 `initial_message` 或附件，调用发送消息服务。
- 更新 `members_count`。

验收：

- 创建者永远是成员。
- 同一个 channel 中成员唯一。
- 首条消息创建后 `latest_message_id` 与 `last_message_at` 正确。

### 发送消息

输入：`channel_public_id`、`sender_username`、`content`、`reply_to_public_id?`、`attachments[]?`。

行为：

- 校验 sender 是 channel member。
- 校验 `content` 非空或附件非空。
- 如果有 `reply_to_public_id`，必须属于同一 channel。
- 创建 message。
- 创建附件并绑定到 message。
- 更新 channel `latest_message_id` 和 `last_message_at`。
- 根据 mention/reply 规则写 `message_notifications`，但不做外部投递；当前已完成首批：回复其他当前成员的消息时，为被回复消息的 sender 写一条内部提醒；消息内容中的 `@username` 只匹配当前 channel 成员并跳过 sender；self-reply、非成员 mention 和重复 recipient 不写。
- 通过事件广播 trait 发出 `message_created`。

验收：

- 非成员不能发送。
- reply_to 跨 channel 被拒绝。
- 附件消息允许 content 为空。
- latest message 始终指向最新未删除消息。

### 编辑消息

输入：`message_public_id`、`actor_username`、`content`。

行为：

- actor 必须是原 sender，或后续明确的管理员权限。
- 更新 content。
- 发出 `message_updated`。

验收：

- 非作者不能编辑。
- soft-deleted message 不能编辑。

### 删除消息

输入：`message_public_id`、`actor_username`。

行为：

- actor 必须是原 sender，或后续明确的管理员权限。
- 写入 `discarded_at`。
- 如果删除的是 channel latest message，回算最新未删除消息。
- 发出 `message_deleted`。

验收：

- 删除 latest message 后 channel 排序字段正确。
- 删除非 latest message 不改变 latest pointer。

### 已读/未读

第一版可以只实现 storage 与内部服务，不暴露 API；如果前端需要，新增：

| 方法 | 路径 | 语义 |
|------|------|------|
| `POST` | `/api/v1/chat/channels/{channel_id}/reads` | 标记当前用户已读。 |
| `DELETE` | `/api/v1/chat/channels/{channel_id}/reads` | 手动标未读。 |

验收：

- mark read 设置当前 membership 的 `last_read_at`。
- mark unread 设置 `manually_marked_unread_at`，并按 Rails 行为把 `last_read_at` 调到 latest message 之前。

## 实时事件

第一版只定义内部 trait，不要求 Pusher 兼容。

```rust
pub trait ChatEvents {
    async fn message_created(&self, channel_public_id: &str, message_public_id: &str);
    async fn message_updated(&self, channel_public_id: &str, message_public_id: &str);
    async fn message_deleted(&self, channel_public_id: &str, message_public_id: &str);
    async fn channel_updated(&self, channel_public_id: &str);
}
```

默认实现可以是 no-op。当前已提供 `InMemoryChatEvents` 进程内 broadcast hub，供测试和后续网关消费；WebSocket/Pusher 兼容网关作为后续切片，不得阻塞 CRUD 切片合入。

## 数据迁移

迁移输入：PlanetScale/MySQL 中范围内 9 张 legacy 表。

迁移输出：monoengine 目标库中的 chat/channel 表。

转换规则：

- `message_threads` 导入为 `channels`。
- `message_thread_memberships` 导入为 `channel_memberships`。
- `message_thread_membership_updates` 导入为 `channel_membership_updates`。
- `organization_membership_id`、`owner_id`、`sender_id`、`actor_id` 通过临时映射转换为 `username`。
- `oauth_application_id`、`integration_id` 相关行如果代表 integration DM 或 app message，默认跳过，并输出计数报告。
- `call_id` 非空的 message 默认跳过或转成系统文本，切片开工前二选一。推荐跳过并输出报告，因为 Calls 不迁移。
- `system_shared_post_id` 非空的 message 默认跳过或转成系统文本，切片开工前二选一。推荐转成不可点击系统文本，避免聊天断层。
- attachments 只迁移 `subject_type = 'Message'` 且 message 成功迁移的行。
- reactions 只迁移 `subject_type = 'Message'` 且 message 成功迁移的行。
- custom reactions 去掉 organization 维度后，若 name 冲突，保留最早创建的一条，其余写入冲突报告。

迁移阶段：

1. 建目标 schema。
2. 导出范围内 9 张 legacy 表。
3. 准备 `legacy_user_id -> username` 和 `legacy_organization_membership_id -> username` 映射。
4. 运行转换导入脚本，产出成功数、跳过数、冲突数、错误样本。
5. 对比 channel 数、message 数、attachment 数、reaction 数。
6. 灰度切读 API。
7. 灰度切写 API。
8. 保留 Rails 回滚路径至少一个发布周期。

阻塞条件：

- 无法建立完整用户映射。
- integration/call/post 派生消息比例高到产品不能接受跳过或降级。
- public_id 冲突未处理。
- latest pointer 校验失败。

## 切片执行计划

### Slice 0: 对齐边界

目标：让代码元数据与本文一致。

任务：

- 确认 `ChatCapability` 只有 `SharedFoundations`、`ChannelChat`。
- 确认 `ChatEntityKind` 只有 `Channel`、`Message`、`Attachment`、`Reaction`、`CustomReaction`、`OpenGraphLink`。
- 确认目标代码和目标文档不使用源项目名称作为命名空间或前缀。
- 更新 `MIGRATION_SLICES` 的 `rust_target`，目标使用 chat/channel 命名。

验收：

- `cargo +nightly fmt --all --check`
- `cargo build`
- `cargo clippy --all-targets --all-features -- -D warnings`

### Slice 1: Shared Foundations schema + storage（主干已落地）

目标：落地附件和 reaction 相关实体与基础 storage。

任务：

- 已完成主干：新增 4 张表的 SeaORM 实体与迁移。
- 已完成主干：新增 attachment storage：创建、按 message 查询、排序。
- 已完成主干：新增 reaction storage：创建、soft delete、按 message 聚合。
- 已完成主干：新增 custom reaction storage：创建、按 name/public_id 查询。
- 已完成主干：新增 open graph storage：按 URL upsert/query。
- 剩余：补完整唯一约束冲突矩阵、附件软删除策略和链接预览抓取/缓存行为。

验收：

- 迁移测试覆盖 4 张表和关键索引。
- ✅ storage 测试覆盖 create/query/soft delete/unique conflict（`custom_reaction_storage::test_custom_reaction_rejects_duplicate_lowercase_name` 覆盖 `lower(name)` 唯一冲突；reactions 部分唯一索引因 Postgres 对 nullable 列的 NULL-distinct 语义实际不强制去重，见下方约束说明）。
- 不引入外部网络调用。

### Slice 2: Channel Chat schema + storage（主干已落地）

目标：落地 channel/message 相关实体与存储层访问控制。

任务：

- 已完成主干：新增 5 张表的 SeaORM 实体与迁移。
- 已完成主干：新增 channel storage：list visible、find visible、create、update、soft delete。
- 已完成主干：新增 membership storage：add/remove/list、mark read/unread。
- 已完成主干：新增 message storage：page、create、update、soft delete、recompute latest。
- 已完成首批：channel/message 读取按 `username` 强制 membership join；2026-06-23 已补 message edit/delete 的 path channel + current membership guard。
- 已完成首批：`chat_router` response mapping 中的 message/custom-reaction 读取已迁回 storage helper。
- 剩余：继续减少其他 handler 内直接 SeaORM 查询，补稳定的 storage 级权限回归测试矩阵。

验收：

- 非成员无法读 channel。
- 非成员无法读 messages。
- latest message 回算测试通过。
- soft-deleted channel/message 默认不可见。

### Slice 3: Channel Chat service（主干已落地）

目标：把 Rails callback 行为显式化。

任务：

- 已完成主干：实现 `create_channel`。
- 已完成主干：实现 `send_message`。
- 已完成主干：实现 `update_message`。
- 已完成主干：实现 `delete_message`。
- 已完成主干：实现 `add_members` / `remove_members`。
- 已完成首批：实现 reply 与 `@username` mention 的 message notification 内部状态写入。
- 已完成首批：接入 no-op 默认 event broadcaster，并新增进程内 `InMemoryChatEvents` broadcaster；WebSocket/Pusher 兼容网关仍为后续。

验收：

- 服务层集成测试覆盖创建 channel + 首条消息。
- 服务层集成测试覆盖 reply、attachment、reaction。
- 服务层集成测试覆盖成员变更记录。

### Slice 4: HTTP API（主干已落地）

目标：暴露绿地 API。

任务：

- 已完成主干：新增 `chat_router` 并挂载到现有 API router。
- 已完成主干：新增 request/response DTO。
- 已完成主干：新增 OpenAPI 标注。
- 已完成主干：接入当前用户 `username` 提取。
- 已完成主干：把 service 错误映射成统一 API error。
- 剩余：补真实 HTTP 黑盒矩阵、稳定无 Redis router 测试，以及是否暴露成员管理 HTTP 端点的产品决策。

验收：

- HTTP 测试覆盖所有第一版端点。
- OpenAPI 构建通过。
- API 不暴露数据库内部 ID，只暴露 public_id 和 username。

### Slice 5: Attachment direct upload（基础已落地）

目标：支持附件预签名和注册。

任务：

- 已完成基础：实现 presign endpoint。
- 已完成基础：实现 attachment confirmation endpoint。
- 已完成首批：校验当前用户对目标 message/channel 的访问权限。
- 已完成首批：校验文件名不含路径分隔/控制字符、文件大小为正且不超过 100MiB、MIME 形态包含 `/`、confirm file_path 必须是 `chat/attachments/` object key 且无 traversal 段。
- 剩余：产品级 MIME allowlist、已上传对象归属/存在性复核和更完整 object storage fake/no-op 测试。

验收：

- 无权限用户不能给别人的 channel 注册附件。
- 注册附件后消息查询能返回附件。
- object storage 在测试中使用 fake/no-op adapter。

### Slice 6: Data migration tooling

目标：能从源 MySQL 导入目标表。

任务：

- 写导入脚本或一次性 CLI 子命令。
- 实现用户映射输入。
- 实现跳过/降级报告。
- 实现校验报告。

验收：

- 可在脱敏 fixture 上完整导入。
- 导入结果 idempotent 或明确要求空库导入。
- 报告包含每张表输入数、输出数、跳过数、冲突数。

## 测试策略

必须覆盖：

- 迁移测试：所有新表可在 PostgreSQL 测试库中 migrate。
- Storage 测试：权限过滤、soft delete、唯一约束、分页排序。
- Service 测试：创建 channel、发送消息、编辑、删除、reply、附件、reaction、成员变更、latest message 回算。
- HTTP 测试：端点状态码、响应 shape、错误映射。
- 导入测试：用小型 fixture 覆盖用户映射、跳过 integration/call/post 派生消息、public_id 保留。

测试命令：

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
source .env.test && cargo test --all
```

如果 `cargo test --all` 因既有长跑测试超时，切片 PR 至少必须运行并记录相关新测试的精确命令，并说明完整测试的超时点。

## Review Checklist

每个实现 PR 必须满足：

- 目标代码、表、API、配置、指标和日志字段不使用源项目名称。
- 没有迁移范围外的表或字段。
- 表名使用业务语义，不加来源前缀。
- 所有用户引用都是 `username`，没有新建源用户表。
- 所有读取和写入路径都校验 channel membership；message edit/delete 必须同时校验 path 中的 channel public_id 与 message 实际 channel 一致。
- 所有 soft delete 表默认过滤 `discarded_at IS NULL`。
- Handler 不直接调用 SeaORM。
- API response 不泄露内部自增 ID。
- 没有引入 Slack、Linear、Figma、HMS、OpenAI、Postmark、Pusher 依赖。
- 新增测试覆盖当前切片 DoD。

## Open Questions

这些问题不阻塞 Slice 0-2，但必须在对应切片前确认：

| 问题 | 最晚确认时间 | 默认方案 |
|------|--------------|----------|
| `call_id` 非空消息如何迁移 | Slice 6 前 | 跳过并报告。 |
| `system_shared_post_id` 非空消息如何迁移 | Slice 6 前 | 转成系统文本。 |
| message content 是否继续存 HTML | Slice 3 前 | 保持 HTML，避免前端渲染迁移。 |
| API 对非成员返回 403 还是 404 | Slice 4 前 | 404，减少资源枚举。 |
| 是否需要 Rails `/v1` 兼容路径 | Slice 4 前 | 不需要，先只做绿地路径。 |
| 是否需要 Pusher 兼容 WebSocket | CRUD 上线后 | 独立切片评估。 |
