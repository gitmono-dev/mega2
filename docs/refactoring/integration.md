# Monoengine 集成测试方案（修订版）

本文档定义 monoengine 的集成测试策略。它的目标不是替代单元测试，而是用接近生产的
PostgreSQL、Redis、SMTP、对象存储和 Vault 启动顺序，验证跨模块数据流、控制流和错误
边界是否正确。

> **治理规范**：本文档遵循 `../general.md` 中定义的统一结构、共同约束和执行标准。
> 在审阅或执行本计划前，请先查阅 `general.md`。

## 事实校准（2026-06-28）

> 本文档中的代码引用、测试名与环境配置已对照 `bin/tests/`、`src/notification/`、`docker-compose.test.yml` 和 `.github/workflows/config-validation.yml` 重新核对。需特别注意以下事实：

1. **工作区已拆分**：monoengine 是 Cargo workspace（`Cargo.toml` `members = ["bin"]`）；`monoengine-core`（lib）仅依赖 `orbit-api`，瘦二进制 `monoengine`（`bin/`）依赖实现 crate。黑盒集成测试位于 `bin/tests/integration_vault.rs`（属 `monoengine` bin crate），共享 helper 在 `bin/tests/common/mod.rs`。
2. **Docker 测试栈已就位**：`docker-compose.test.yml` 定义 PostgreSQL 15（`postgres:15-alpine`，host 端口 15432）、Redis 7（`redis:7-alpine`，16379）、Mailpit（`axllent/mailpit:v1.27`，11025/18025）；使用高位 host 端口避免冲突；无外部 Vault 容器（嵌入式 `VaultCore`）。
3. **P0 黑盒 CLI/HTTP 门禁已落地**（`bin/tests/integration_vault.rs`）：`config_secret_ref_does_not_load_config`、`config_secret_set_check_and_validate_resolve_secret`、`config_validate_resolve_secrets_fails_when_secret_is_missing`、`config_secret_ref_rejects_bootstrap_secret_fields`、`integration_service_http_smoke`、`integration_service_http_fails_when_mailer_secret_missing`、`integration_config_init_creates_safe_skeleton_and_validates`。
4. **P1 脱敏与端到端门禁已落地**：脱敏工具 `src/config/redaction.rs` 已实现；活进程门禁 `integration_error_redaction_does_not_leak_db_password`/`..._redis_password`/`..._bad_toml_does_not_leak_values`（`bin/tests/`）断言凭据不泄露；邮件 outbox→Mailpit 端到端由 `integration_mail_dispatcher_mailpit_sends_outbox_job`（`src/notification/dispatcher.rs`）覆盖；CL 评论触发器→enqueue/render 由 `test_on_cl_comment_created_*`（`src/notification/triggers.rs`）覆盖。
5. **多渠道通知与对象存储已落地（2026-06-27/2026-06-28）**：`NotificationChannel` 抽象支持 slack/webhook/in-app；webhook 扇出由 `service_start_fans_out_delivery_to_webhook_channel`（`src/notification/service.rs`）覆盖；**P2 黑盒 gate `integration_multichannel_notification` 已落地（2026-06-28）**：启动真实 `service http`，通过写入 `email_jobs` outbox 触发 dispatcher，验证 email/Mailpit、in-app、Slack、webhook 均收到扇出；对象存储 `vault://` SecretRef 在 post-vault 启动路径解析（`src/context/mod.rs`），并在 `config validate` / `config secret set/check` / `config validate --resolve-secrets` 中对齐（`src/config/validate.rs`、`src/commands/config.rs`）。
6. **CI 门禁**：`.github/workflows/config-validation.yml` 运行 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、配置校验单测与 `cargo test -p monoengine --test integration_vault`、`monoengine-core` 的 `notification::{dispatcher,service,triggers}` 集成测试（启动 postgres+redis+mailpit、`::add-mask::` 凭据脱敏）。

## 审查结论

原方案的方向合理：确实应该用容器化依赖验证 `Config -> DB(migrations) -> Vault ->
对象存储 -> Storage -> Redis -> Mail -> Notification -> service` 这条链路。但原方案混淆了当前实现和未来目标，且存在几个
会导致测试验证错误对象的关键问题：

1. **Vault 模型不正确**：monoengine 当前使用嵌入式 `VaultCore` + 数据库存储，不依赖外部
   HashiCorp Vault 服务；测试环境不应启动 `vault` 容器，也不应使用
   `VAULT_DEV_ROOT_TOKEN_ID`。
2. **数据库初始化方式不正确**：不能手写 `ci/sql/init-test-db.sql` 的 schema 子集。真实
   schema 来自 `src/jupiter/migration/*`，`Storage::new` 会调用
   `database_connection()` 并执行 pending migrations。手写 SQL 会绕过约束、索引、字段名和
   迁移顺序，尤其会错误建出 `email_jobs.recipient` 这类当前实体并不存在的列。
3. **CLI 能力分层**：当前已有 `config secret ref/set/check/rotate`、
   `config validate [--resolve-secrets]` 以及 `config init`（`config init` 已实现并由
   `config-validation.yml` 的 CI 步骤覆盖）。专门的黑盒用例 `integration_config_init_creates_safe_skeleton_and_validates` 已单列（`bin/tests/integration_vault.rs`）。
4. **功能边界过宽**（原始分析；部分已落地）：多渠道通知与 Slack/webhook/in-app 投递**已落地**（`NotificationChannel` 抽象 + dispatcher 扇出，2026-06-27 已补模块集成测试，见下表「可行性」行），热加载白名单与用户通知 API 接口亦已实现（单测充分）；其专门的黑盒进程 gate 仍可作为后续 P2 单列，但不放进当前 P0 集成测试 gate。
5. **二进制 crate 测试边界**：本仓库为 Cargo workspace——库 crate `monoengine-core`
   （`src/lib.rs`）+ 瘦二进制 crate `monoengine`（`bin/src/main.rs`，见 `orbit.md`）。
   黑盒集成测试位于 `bin/tests/integration_*.rs`，通过
   `CARGO_BIN_EXE_monoengine`/`std::process::Command` 调用 CLI 和 HTTP，必须用
   `cargo test -p monoengine --test <name>` 运行（默认成员是 `monoengine-core` 库）；
   需要内部模块访问的“模块集成测试”保留在 `src/**::tests`（随 `monoengine-core` 编译，
   可直接 `use crate::...`）。
6. **安全验收过早声明**：脱敏工具（`src/config/redaction.rs`）已落地——`database_connection()`
   现用 `redact_db_url` 记录脱敏连接串，Redis init 错误用 `redact_redis_url` 包装，因此场景 7 的
   脱敏 gate 已可声称满足并已有活进程门禁覆盖 DB/Redis 凭据不泄露。

因此，本修订版采用“**当前可执行 P0** + **P1/P2 扩展 gate**”的分层策略。

## 改进方案多维评估小结

| 维度 | 评估 | 文档修订决策 |
| --- | --- | --- |
| 合理性 | 中高。跨模块端到端测试方向正确，但原方案把外部 Vault 和手写 schema 当作真实依赖，偏离当前架构。 | 改为嵌入式 Vault、真实 migrations、真实启动顺序。 |
| 可行性 | **（2026-06-27）多渠道通知与对象存储 SecretRef 已补模块集成测试**：`src/notification/service.rs::tests::service_start_fans_out_delivery_to_webhook_channel` 用本地 axum server 验证 `WebhookChannel` 作为 secondary 渠道在 email 主投递成功后收到 dispatcher 扇出（Slack 渠道为同一 `NotificationChannel` 抽象的 webhook 变体）；`src/context/mod.rs::tests` 验证 `object_storage.s3` 凭据的 `vault://` SecretRef 在 post-vault 解析（字面量透传 / 单&双 ref / 缺失报错不 panic）。workspace 拆分后黑盒测试位于 `bin/tests/`（`monoengine` bin crate，不导入内部模块），模块集成测试在 `monoengine-core` 内可 `use crate::`。 | 分成黑盒进程测试、模块集成测试和未来 gate。 |
| 完整性 | 覆盖面广但缺少测试夹具、隔离、端口冲突、fallback 检测、超时与清理策略。 | 增加环境隔离、数据隔离、超时、清理和覆盖矩阵。 |
| 安全性 | 原方案要求脱敏但未指出彼时 DB URL 的泄露风险；外部 Vault root token 反而增加误导。 | 明确禁用外部 Vault 容器，secret 只经 stdin；日志脱敏 P1 gate 已落地（DB/Redis 连接串脱敏 + 活进程门禁）。 |
| 功能正确性与接口兼容性 | `SecretRef`、mail.password、**多渠道通知（Slack/webhook 渠道 + dispatcher 扇出，2026-06-27 已落地并补模块集成测试）** 与对象存储 SecretRef 路径均已对接当前代码；`config init` 黑盒用例已落地（`integration_config_init_creates_safe_skeleton_and_validates`），热加载黑盒用例（接口已实现，单测充分）仍可作为后续 P2 gate 单列。 | 当前 gate 只使用已存在 CLI 和 HTTP/service 接口。 |
| 数据流与控制流正确性 | 原方案的主链路大体正确，但忽略实际启动顺序（DB+migrations → DB-backed Vault → 对象存储 SecretRef 解析/构造 → Storage → Redis → mail 在 Vault 后构造）。 | 明确启动顺序和每类测试允许触达的依赖。 |
| 性能与效率 | 原方案每次可能重建容器、重跑 release build，成本高。 | 复用 compose stack，测试使用 dev/test binary，按测试隔离 DB/schema。 |
| 可靠性与容错性 | 原方案依赖固定 sleep/默认端口，且没有明确数据库连接目标。 | 使用 healthcheck + 主动探测，测试必须证明连接的是 PostgreSQL。 |
| 兼容性与互操作性 | Docker Compose 方向可行，但默认端口易与开发机冲突；SMTP 捕获服务 API 与 TLS 配置需明确。 | 使用高位 host 端口，SMTP 测试关闭 STARTTLS，CI 以 Linux 为基线。 |
| 可扩展性与可维护性 | 原文场景多但优先级不清，后续容易把未实现功能写成失败测试。 | 用 P0/P1/P2 gate 管理扩展，新增功能先更新矩阵再落测试。 |
| 合规性与标准符合性 | 需要遵守仓库必跑 gate、GitHub Actions secret masking、测试数据清理。 | 增加执行 gate、mask、临时目录和日志保留规则。 |

## 当前实现状态速览表

| 能力 | 当前状态 | 集成测试策略 |
| --- | --- | --- |
| 配置加载 | 已实现 `Config::new`、env overlay、`config validate` 基础校验 | P0 黑盒 CLI 测试 |
| CLI 两阶段加载 | 已实现 `LoadMode`；`config secret ref` 不加载配置，`set/check` 走最小 DB/Vault bootstrap | P0 黑盒 CLI 测试 |
| `config init` | 已实现（生成安全骨架配置，由 `config-validation.yml` CI 覆盖） | 黑盒用例 `integration_config_init_creates_safe_skeleton_and_validates` 已落地 |
| Vault | 嵌入式 `VaultCore`，通过 DB + `core_key.json` 管理；无外部 Vault 服务 | P0 使用 DB 和临时 `MEGA_BASE_DIR` |
| SecretRef | 已支持 `vault://secret/<name>#<field>`；`config secret` 可写入 `mail.password`、`notification.slack.webhook_url`、`notification.webhook.token`、`object_storage.s3.access_key_id`、`object_storage.s3.secret_access_key`（见 `SUPPORTED_SECRET_FIELDS`）；对象存储 `vault://` 在启动路径、`config validate`、`config secret set/check`、`--resolve-secrets` 中均已对齐 | P0 覆盖 `ref/set/check/validate --resolve-secrets` 与对象存储 S3 凭据 |
| 数据库 | `database_connection()` 只支持 PostgreSQL，连接后自动执行 migrations | P0 必须检测真实连接到 PostgreSQL |
| Redis | `AppContext::new` 在 Vault/对象存储/Storage 之后初始化 Redis | P0 service smoke 需要 Redis 容器 |
| 对象存储 | 通过 `jupiter::storage::object_storage::build_object_storage` 构造（由 composition root 注入 `Storage::new`），测试可使用 local temp dir | P0 使用 local backend |
| Mail | `mail.password_ref` 已可在 Vault 后解析；`SmtpMailer` 在 `AppContext::new` 中构造；`integration_mail_dispatcher_mailpit_sends_outbox_job` 已覆盖真实 SMTP/Mailpit 正路径，`integration_mail_dispatcher_smtp_failure_retries_outbox_job` 已覆盖 SMTP transport 失败 retry | P0/P1 扩展 Mailpit/SMTP 故障矩阵 |
| Notification | email outbox、dispatcher、CL comment trigger、**用户-facing 偏好 API 与多渠道（Slack/webhook/in-app + dispatcher 扇出）均已实现**（2026-06-27 已补模块集成测试） | P1 模块集成 + service dispatcher 测试 |
| 热加载 | 已实现（`config::reload` 的 `ConfigHandle`/白名单应用/`ConfigReloadWatcher` + 日志/mail dispatcher/template/mailer 订阅者，单测充分） | **（2026-06-28）P2 专门黑盒 `integration_config_hot_reload` 已落地**：基于 base/profile 文件 watcher 触发，验证白名单字段（`log.level`）热生效、非白名单字段（`monorepo.import_dir`）仅报告 restart-required 且旧配置继续服务、非法 TOML 被拒绝且服务不中断 |
| 日志脱敏 | 已落地：DB/Redis 连接串经 `redact_db_url`/`redact_redis_url` 脱敏，活进程门禁断言凭据不泄露 | P1 gate 已满足；可继续收拢成单一命名 gate |

## 硬约束与不可违反的原则

本文档中以下约束是硬边界，任何实现偏离都必须重新评审：

1. **禁止外部 Vault 容器。** 测试环境必须使用嵌入式 `VaultCore`（数据库存储 + 进程内 `core_key.json`），不得启动外部 HashiCorp Vault 或使用 `VAULT_DEV_ROOT_TOKEN_ID`。理由：monoengine 架构不依赖外部 Vault；用外部 Vault 会测试错误的系统形态。
2. **必须用真实 migrations 初始化数据库。** 禁止手写 schema 子集（如 `init-test-db.sql`）；所有表/列/约束来自 `src/jupiter/migration/*`（经 `database_connection()` 自动执行）。理由：手写 schema 会绕过约束与迁移顺序，导致测试通过但生产 schema 不匹配。
3. **黑盒集成测试必须通过真实二进制。** CLI/HTTP 黑盒测试位于 `bin/tests/integration_*.rs`，经 `CARGO_BIN_EXE_monoengine` + `std::process::Command` 调用真实二进制，不得 `use crate::...` 导入内部模块。理由：mock 路径无法验证启动顺序、退出码、日志脱敏等真实行为。
4. **启动顺序硬约束。** `Config → database_connection()（执行 migrations）→ DB-only VaultCore bootstrap → resolve object_storage SecretRef → build 对象存储 + Storage::new_with_connection → Redis → Mail → Notification → HTTP`（实际顺序见 `AppContext::new`；migrations 在 `database_connection()` 内、vault bootstrap 之前完成）。理由：顺序改变会影响 secret 可用性与依赖解析。
5. **Secret 值禁止出现在 CLI 参数、日志、panic 中。** 只能经 stdin、受权限保护的文件或 CI secret 注入；日志须经脱敏工具（`redact_db_url`/`redact_redis_url`）。理由：防止凭据泄露到日志、版本控制、事故回放。
6. **测试数据隔离与清理。** 每个测试使用独立临时目录，并通过独立数据库或 schema 隔离（黑盒进程测试建独立数据库，模块测试用 schema）；结束后删除 `core_key.json` 与临时配置。理由：防止测试间污染、凭据复用、密钥文件泄露。
7. **三项 gate（本地与 CI 等价）。** 本地必须通过 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test`（指定相关用例）；CI（`config-validation.yml`）以等价方式执行——直接设置 `MEGA_DATABASE__DB_URL`/`MEGA_REDIS__URL`/`MAILPIT_API_URL` 等环境变量后运行 `cargo test -p monoengine --test integration_vault` 与 `monoengine-core` 的 `notification::{dispatcher,service,triggers}` 过滤测试。理由：保持验收标准统一、可回归。

## 现状与目标对比

| 维度 | 当前状态 | 目标状态 | 实现难度 |
|-----|--------|--------|--------|
| CLI secret 黑盒门禁 | `config_secret_ref_does_not_load_config`/`config_secret_set_check_and_validate_resolve_secret`/`config_validate_resolve_secrets_fails_when_secret_is_missing`/`config_secret_ref_rejects_bootstrap_secret_fields` 已落地 | 维持作为稳定 P0 gate | 简单 |
| 服务 HTTP 启动链路 | `integration_service_http_smoke` + `integration_service_http_fails_when_mailer_secret_missing` 已落地 | 维持启动顺序与 fail-closed 断言 | 简单 |
| `config init` 黑盒 | `integration_config_init_creates_safe_skeleton_and_validates` 已落地（CI 亦覆盖） | 持续随模板演进 | 简单 |
| 邮件投递与故障矩阵 | `integration_mail_dispatcher_mailpit_sends_outbox_job` + 多个 SMTP 失败/dead-letter/skip 用例已落地（模块集成测试） | 扩展长时压力与更完整故障矩阵（mail.md 阶段 5，部分 deferred） | 中等 |
| 通知触发器→邮件 | `test_on_cl_comment_created_*`（enqueue/render/opt-out）已覆盖 | 维持触发器到 outbox 的端到端断言 | 中等 |
| 多渠道扇出 | `service_start_fans_out_delivery_to_webhook_channel` 模块集成测试已落地；**（2026-06-28）P2 黑盒 `integration_multichannel_notification` 已落地**：启动真实 `service http`，配置 mail + slack + webhook，通过直接写入 `email_jobs` outbox 触发 dispatcher，验证 email（Mailpit）、in-app（`user_inbox_notifications`）、Slack 与 webhook 均收到扇出 | 维持作为 P2 gate | 中等 |
| 错误诊断与脱敏 | `integration_error_redaction_does_not_leak_db_password`/`..._redis_password`/`..._bad_toml_does_not_leak_values` 活进程门禁已落地 | 可继续收拢成单一命名 gate | 中等 |
| 热加载黑盒 | **（2026-06-28）P2 黑盒 `integration_config_hot_reload` 已落地**：基于 base/profile 文件 watcher 触发，覆盖白名单热生效、restart-required 继续服务、坏 TOML 拒绝回退 | P2：GCS 凭据/真实 S3 后端进程级 gate 仍为后续 | 中等 |
| 对象存储 SecretRef | 启动路径解析有模块测试（透传/双 ref/缺失报错）；validate/CLI/`--resolve-secrets` 已与 S3 凭据对齐；P2 进程级黑盒 gate `config_secret_set_check_and_validate_resolve_object_storage_s3_secret_refs` 已落地，覆盖 `config secret set/check` 与 `config validate --resolve-secrets` 对 S3 凭据的端到端解析（不依赖真实 S3 服务端） | P2：GCS 凭据/真实 S3 后端进程级 gate 仍为后续 | 复杂 |
| 覆盖矩阵 | 覆盖矩阵表已维护，P0/P1 gate 稳定 | 新功能先更新矩阵再落测试 | 简单 |

## 测试分层

### 1. 单元测试（当前已有）

位置：`src/**/*.rs` 中的 `#[cfg(test)] mod tests`。

用途：
- 解析、校验、SecretRef、resolver cache、storage 业务逻辑。
- 允许直接访问 crate 内部模块。
- 纯逻辑测试不依赖 Docker；涉及数据库的测试使用 PostgreSQL 测试环境，优先通过独立数据库或 schema 隔离。

### 2. 模块集成测试（crate 内部）

位置：放在相关模块的 `#[cfg(test)]` 中（随 `monoengine-core` 库编译，可直接
`use crate::...`）。

用途：
- 需要调用 `crate::jupiter::migration::apply_migrations`、`NotificationStorage`、
  `on_cl_comment_created` 等内部 API 的测试。
- 可连接 Docker PostgreSQL，但必须由测试显式配置，不依赖开发机默认服务。

### 3. 黑盒集成测试（进程级）

位置：`bin/tests/integration_*.rs`（属于 `monoengine` 二进制 crate）。

限制：
- 黑盒测试通过 `CARGO_BIN_EXE_monoengine` 拉起真实二进制，不导入 `crate::...`。
- 只能通过 `std::process::Command` 调用 `CARGO_BIN_EXE_monoengine`、HTTP API、SMTP/Mailpit
  API、PostgreSQL/Redis 客户端协议进行断言。

用途：
- CLI 行为、启动顺序、HTTP smoke、secret 不回显、进程退出码。
- 不验证内部函数细节。

## 测试环境架构

### 容器编排文件：`docker-compose.test.yml`

不要包含 Vault 容器。Vault 是 monoengine 进程内组件，状态来自数据库和测试用
`MEGA_BASE_DIR` 下的 `core_key.json`。

```yaml
services:
  postgres:
    image: postgres:15-alpine
    environment:
      POSTGRES_USER: mono
      POSTGRES_PASSWORD: mono_test_password
      POSTGRES_DB: monoengine_it
    ports:
      - "127.0.0.1:15432:5432"
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U mono -d monoengine_it"]
      interval: 2s
      timeout: 5s
      retries: 20

  redis:
    image: redis:7-alpine
    ports:
      - "127.0.0.1:16379:6379"
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 2s
      timeout: 5s
      retries: 20

  mailpit:
    image: axllent/mailpit:v1.27
    ports:
      - "127.0.0.1:11025:1025"
      - "127.0.0.1:18025:8025"
    healthcheck:
      test: ["CMD", "wget", "--quiet", "--tries=1", "--spider", "http://localhost:8025"]
      interval: 2s
      timeout: 5s
      retries: 20

networks:
  default:
    name: monoengine-test-net
```

### 配置隔离

每个测试必须使用独立临时目录：

- `MEGA_BASE_DIR=<temp>/base`
- `MEGA_CONFIG=<temp>/config.toml`
- `object_storage.local.root_dir=<temp>/objects`
- `database.db_path=""`，保留兼容字段但不参与 PostgreSQL 连接
- `log.print_std=true` 或日志输出到 `<temp>/logs`

PostgreSQL 连接串必须指向高位端口：

```toml
[database]
db_type = "postgres"
db_url = "postgres://mono:mono_test_password@127.0.0.1:15432/monoengine_it"
max_connection = 8
min_connection = 1
connect_timeout = 5
idle_timeout = 60
sqlx_logging = false
sqlx_logging_level = "warn"
db_path = ""

[redis]
url = "redis://127.0.0.1:16379"

[object_storage]
storage_type = "local"

[object_storage.local]
root_dir = "/tmp/monoengine-it/objects"

[mail]
enabled = true
smtp_host = "127.0.0.1"
smtp_port = 11025
from = "no-reply@example.test"
starttls = false
password_ref = "vault://secret/config/it/mail/password#value"
```

### 数据库初始化

禁止手写 schema 初始化脚本。测试必须通过真实 migrations 建表：

- 进程级服务启动：`Storage::new` 会调用 `database_connection()`，并执行
  `apply_migrations(&conn, false)`。
- 内部模块测试：直接使用 `crate::jupiter::migration::apply_migrations`。
- 黑盒测试如需断言表结构，只查询 migrations 后的真实表，如
  `email_jobs(username, to_email, event_type_code, subject, body_html, ...)`。

必须增加一个 PostgreSQL 确认断言，防止测试误连到非目标数据库。可通过以下方式之一确认：

- 通过 PostgreSQL 连接查询 `SELECT current_database()` 并确认测试数据出现在该数据库。
- 为每个测试创建独立 database/schema，并在断言时使用同一连接串查询目标表。

## P0：当前必须可执行的集成测试

### 1. CLI secret 引用生成（`integration_cli_secret_ref`）

**目标**：验证不需要配置、不需要数据库、不需要 Vault 的纯引用生成。

**步骤**：
1. 执行：
   ```bash
   monoengine config secret ref mail.password --vault-path config/it/mail/password --field value
   ```
2. 捕获 stdout/stderr 和退出码。

**验收标准**：
- 退出码为 0。
- stdout 等于 `vault://secret/config/it/mail/password#value`。
- 不读取 `MEGA_CONFIG`，即使配置文件不存在也成功。
- `database.db_url`、secret value 等敏感内容不出现在输出中。

### 2. 最小 DB/Vault bootstrap（`integration_cli_secret_set_check`）

**目标**：验证 `config secret set/check` 只依赖数据库和嵌入式 Vault，不初始化 Redis、
object storage、mail 或 HTTP 服务。

**步骤**：
1. 启动 PostgreSQL。
2. 写入只包含 `[database]` 的最小配置文件。
3. 通过 stdin 写入 secret：
   ```bash
   printf '%s' 'smtp-test-password' \
     | monoengine --config <temp>/config.toml \
         config secret set mail.password \
         --vault-path config/it/mail/password \
         --field value \
         --value-stdin
   ```
4. 执行：
   ```bash
   monoengine --config <temp>/config.toml \
     config secret check mail.password \
     --ref vault://secret/config/it/mail/password#value
   ```

**验收标准**：
- 两条命令均成功。
- stdout 只包含 SecretRef 或 `ok ...`，不包含 `smtp-test-password`。
- Redis 未启动时命令仍成功，证明没有走完整 `AppContext`。
- 数据库存储中创建 Vault 所需数据，且 `core_key.json` 位于测试临时目录。

### 3. 配置验证与 secret 解析（`integration_config_validate_resolve_secrets`）

**目标**：验证 `config validate --resolve-secrets` 通过最小 DB/Vault bootstrap 解析
`mail.password_ref`。

**步骤**：
1. 使用完整测试配置，其中 `[mail] enabled=true` 且设置 `password_ref`。
2. 先用 `config secret set` 写入对应 secret。
3. 执行：
   ```bash
   monoengine --config <temp>/config.toml config validate --resolve-secrets
   ```
4. 删除或改错 secret 字段后再次执行。

**验收标准**：
- secret 存在时输出 `config valid`。
- secret 缺失、字段缺失或 URI 非法时返回非 0，并给出可诊断错误。
- 错误链不输出 secret 明文。
- 当前仅支持 `mail.password`；尝试 `database.db_url`、`redis.url`、
  `object_storage.s3.secret_access_key` 必须被拒绝。

### 4. 服务启动 smoke（`integration_service_http_smoke`）

**目标**：验证真实启动顺序：
`Config -> database_connection(migrations) -> DB-only VaultCore bootstrap -> resolve object_storage SecretRef ->
build object storage + Storage::new_with_connection -> Redis -> mail resolver ->
SmtpMailer/EmailDispatcher -> init_monorepo -> HTTP`。

**步骤**：
1. 启动 PostgreSQL、Redis、Mailpit。
2. 写入完整测试配置，object storage 使用 local temp dir，mail 使用 Mailpit。
3. 先通过 `config secret set` 写入 `mail.password`。
4. 启动：
   ```bash
   monoengine --config <temp>/config.toml service http --host 127.0.0.1 -p <free-port>
   ```
5. 等待端口可连接，调用一个稳定的健康/文档/smoke endpoint。
6. 终止进程，确认退出过程不留下后台子进程。

**验收标准**：
- 服务成功绑定端口。
- 数据库 migrations 已执行。
- Redis 连接成功。
- Mailer 初始化失败时进程应失败并给出可诊断错误，而不是静默禁用。
- 日志中不应出现数据库连接到非 PostgreSQL 的记录。

**当前落地状态**：`bin/tests/integration_vault.rs` 已落地两个进程级用例。
`integration_service_http_smoke` 先用 `config secret set` 写入 `mail.password`，再在空闲端口启动真实
`service http`：用裸 HTTP/1.1 探活后命中 `/api/openapi.json`（断言 200），查询测试库
`seaql_migrations` 证明 migrations 已执行（同时证明确实连接 PostgreSQL），用 SIGINT 触发 CLI 的
ctrl-c handler 优雅退出（退出码 0、不留后台子进程），并断言日志不出现 `sqlite`、不泄露 mail secret。
`integration_service_http_fails_when_mailer_secret_missing` 在不写入 secret 时启动服务，验证启动期
mail resolver fail-closed：进程非 0 退出、给出脱敏的 “secret not found” 诊断（不泄露 vault path/ref/明文）、
且 HTTP 端口从不绑定。两个用例复用既有 Docker Compose PostgreSQL/Redis/Mailpit 测试环境。

### 5. 邮件 outbox 投递（`integration_mail_dispatcher_mailpit`）

**目标**：验证 service 启动后，`EmailDispatcher` 能从 `email_jobs` outbox 投递到 Mailpit。

**步骤**：
1. 启动服务。
2. 通过 PostgreSQL 连接插入：
   - `notification_event_types` 事件类型。
   - 一条 `email_jobs(status='pending', to_email='alice@example.test', ...)`。
3. 轮询 `http://127.0.0.1:18025/api/v1/messages`。
4. 查询 `email_jobs` 状态。

**验收标准**：
- Mailpit 收到邮件。
- `email_jobs.status` 变为 `sent`，`sent_at` 非空。
- 缺少收件人时 job 变为 `skipped`。
- SMTP 临时失败时 job 进入 retry 状态，`retry_count` 增加，`next_retry_at` 有值。

**当前落地状态**：`src/notification/dispatcher.rs::tests::integration_mail_dispatcher_mailpit_sends_outbox_job` 已覆盖 outbox pending job 经真实 `SmtpMailer` 投递到 Mailpit 后进入 `sent` 且写入 `sent_at` 的正路径；`integration_mail_dispatcher_smtp_failure_retries_outbox_job` 已覆盖真实 SMTP transport 连接失败后 job 回到 `pending`、`retry_count` 增加且写入 `next_retry_at`。缺少收件人的状态转换已有 dispatcher 模块测试覆盖；Mailpit/SMTP 故障矩阵已扩展到 7 个集成用例（正路径投递、SMTP 连接失败 retry、协议拒绝 retry、认证拒绝 retry、relay 拒绝 retry、dead-letter、missing-recipient skip），均不泄露凭据。该 Mailpit 正路径用例已改为在 `MAILPIT_API_URL` 不可达时优雅跳过（`eprintln` 提示后 `return`，不再 `panic!`），未启动测试栈时不会让整个测试二进制失败。

## P1：安全与业务链路扩展 gate

### 6. 通知触发器到邮件（`integration_notification_trigger_to_mail`）

当前没有用户-facing notification API，因此该测试不应伪造 HTTP 端点。可选实现方式：

- **模块集成测试**：放在 `src/notification` 或 `src/jupiter/storage` 的测试模块内，使用真实
  PostgreSQL，调用 `on_cl_comment_created`，再用 dispatcher 或 service 投递。
- **黑盒服务测试**：通过数据库插入必要 CL/reviewer/user preference 数据，再触发现有真实业务
  API。如果没有真实 API，本项保持 P1 待实现。

本场景（场景 5 邮件 outbox 投递）的验收范围聚焦 email 渠道；Slack/webhook/in-app 渠道已实现并由 `service_start_fans_out_delivery_to_webhook_channel`（webhook 扇出）与 in-app/服务端到端单测覆盖，其专门的进程级黑盒 gate 仍属 P2。

**当前落地状态**：已按"模块集成测试"方式落地
`src/notification/dispatcher.rs::tests::integration_notification_trigger_to_mail_delivers_via_mailpit`。
该用例在真实 PostgreSQL 上插入 CL（author=alice）并写入用户 settings，以 actor=carol 调用
`on_cl_comment_created`，断言触发器恰好为 author 入队一封 `email_jobs`；随后用真实 `SmtpMailer`
执行 `EmailDispatcher::tick_once()` 投递到 Mailpit，并断言触发器生成的主题（用唯一 CL link 隔离）被
Mailpit 收到、`email_jobs` 终态为 `sent` 且 `sent_at` 非空。Mailpit 不可达时与其它 Mailpit 用例一致优雅跳过。

### 7. 错误诊断与脱敏（`integration_error_redaction`）

这是安全 gate。统一 redaction 工具（`src/config/redaction.rs`）已落地并接入 DB/Redis/通知错误链路，
因此该 gate 已可启用。

**应覆盖**：
- 坏 TOML：错误包含配置路径和字段路径，不 panic。
- 错误 DB URL：日志和 stderr 不包含密码。
- 错误 Redis URL：日志和 stderr 不包含密码。
- 错误 SMTP 密码：日志不包含明文。
- Vault key 丢失：fail-closed，不清空 Vault 数据，不重新生成破坏性状态。

**当前落地状态**：统一 redaction 工具已落地——`database_connection()`（`src/jupiter/storage/init.rs`）
现在用 `redact_db_url` 记录脱敏后的连接串，`init_connection()`（`src/jupiter/redis/mod.rs`）的错误用
`redact_redis_url` 包装。活进程门禁已落地并收拢为统一命名 gate：`bin/tests/integration_vault.rs::integration_error_redaction_does_not_leak_db_password`
用坏 DB URL（哨兵密码）启动 `service http`，断言进程 fail-closed 且日志/stderr 不含明文密码；
`integration_error_redaction_does_not_leak_redis_password` 用好 DB + 坏 Redis URL 验证 Redis 凭据不泄露；
`integration_error_redaction_bad_toml_does_not_leak_values` 用坏 TOML（哨兵 secret）验证配置解析错误
只输出文件路径和字段路径，不泄露哨兵值。SMTP 密码不入日志由
`src/notification/dispatcher.rs` 的多个凭据不泄露用例覆盖；Vault key 丢失 fail-closed 由
`vault_core.rs::tests::test_vault_fails_closed_after_key_file_loss` 覆盖。5 个 bullet 中 3 个已有
活进程黑盒门禁，其余 2 个由模块级集成测试覆盖。

## P2：未来能力 gate

`config init` 黑盒用例已落地；其余能力不属于当前可执行 gate，只有当对应功能实现并稳定后，才新增集成测试。

| 能力 | 前置条件 | 验收方向 |
| --- | --- | --- |
| `config init` | 已落地（黑盒） | 无 DB/Redis/Vault 时生成无真实 secret 的配置骨架；`bin/tests/integration_vault.rs::integration_config_init_creates_safe_skeleton_and_validates` 覆盖 |
| 热加载 | 明确 SIGHUP 或 HTTP API、白名单字段、失败回滚语义 | 白名单生效，非白名单拒绝，旧配置继续服务 |
| 多渠道通知 | `NotificationChannel` trait、Slack/webhook/in-app outbox 与用户偏好 API | 每渠道独立投递、重试、禁用策略 |
| 用户通知设置 API | DTO/router/权限策略实现 | 用户可查询/修改偏好，系统必发事件不可关闭 |
| 对象存储 SecretRef | `Storage::new` 拆成 DB-only -> Vault -> resolve -> full storage | S3/GCS 凭据从 Vault 解析，最小 bootstrap 不依赖对象存储 |

## 执行方式

### 本地快速运行

```bash
docker compose -f docker-compose.test.yml up -d

docker compose -f docker-compose.test.yml exec postgres pg_isready -U mono -d monoengine_it
docker compose -f docker-compose.test.yml exec redis redis-cli ping
curl -fsS http://127.0.0.1:18025/api/v1/messages >/dev/null

source .env.test
# CLI 黑盒集成测试（config secret ref/set/check、validate）位于 bin/tests/integration_vault.rs：
cargo test -p monoengine --test integration_vault -- --nocapture --test-threads=1
# 邮件/通知 dispatcher 端到端（真实 Mailpit + Postgres）、NotificationService 投递、
# CL 评论触发器 enqueue/render，位于 crate 内集成测试：
cargo test -p monoengine-core 'notification::dispatcher::tests::integration_mail_dispatcher' -- --nocapture
cargo test -p monoengine-core 'notification::service::tests' -- --nocapture
cargo test -p monoengine-core 'notification::triggers::tests' -- --nocapture

docker compose -f docker-compose.test.yml down -v
```

> **测试文件命名说明（2026-06-19；2026-06-22 更新：workspace 拆分后该文件随 `bin/` 一并丢失，已从历史恢复到 `bin/tests/integration_vault.rs` 并修复 CI 引用）**：P0 CLI 黑盒场景（`integration_cli_secret_*`，函数名 `config_secret_*`）实现在 `bin/tests/integration_vault.rs`（沿用既有 `VaultCliEnv`/`isolated_command` 辅助），并非独立的 `tests/integration_cli.rs`；workspace 拆分后该文件随 `monoengine` 二进制 crate 编译，用 `cargo test -p monoengine --test integration_vault` 运行。服务级邮件投递与触发器端到端校验以 crate 内集成测试形式存在（`notification::service`、`notification::dispatcher::integration_mail_dispatcher_*`、`notification::triggers`），对真实 Postgres/Mailpit 实跑，已接入 `.github/workflows/config-validation.yml` 的「Run integration tests」步骤（含 redis/mailpit 启动与 `::add-mask::` 凭据脱敏）。

P0 的 CLI 黑盒测试已位于 `bin/tests/integration_vault.rs`；新增黑盒测试放在 `bin/tests/` 下。不要为了集成测试新增不必要依赖；
首版可以只用 `std::process::Command`、`std::net::TcpStream`、`reqwest`、`sea-orm` 和现有依赖。

### CI 示例

```yaml
name: Integration Tests

on: [push, pull_request]

jobs:
  integration:
    runs-on: ubuntu-latest

    services:
      postgres:
        image: postgres:15-alpine
        env:
          POSTGRES_USER: mono
          POSTGRES_PASSWORD: mono_test_password
          POSTGRES_DB: monoengine_it
        ports:
          - 15432:5432
        options: >-
          --health-cmd "pg_isready -U mono -d monoengine_it"
          --health-interval 2s
          --health-timeout 5s
          --health-retries 20

      redis:
        image: redis:7-alpine
        ports:
          - 16379:6379
        options: >-
          --health-cmd "redis-cli ping"
          --health-interval 2s
          --health-timeout 5s
          --health-retries 20

      mailpit:
        image: axllent/mailpit:v1.27
        ports:
          - 11025:1025
          - 18025:8025

    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - name: Mask integration secrets
        run: |
          echo "::add-mask::mono_test_password"
          echo "::add-mask::smtp-test-password"

      - name: Required checks
        run: |
          cargo +nightly fmt --all --check
          cargo clippy --all-targets --all-features -- -D warnings
          source .env.test && cargo test --all

      - name: Integration tests
        env:
          MEGA_DATABASE__DB_TYPE: postgres
          MEGA_DATABASE__DB_URL: postgres://mono:mono_test_password@127.0.0.1:15432/monoengine_it
          MEGA_REDIS__URL: redis://127.0.0.1:16379
          MAILPIT_API_URL: http://127.0.0.1:18025
        run: |
          cargo test -p monoengine --test integration_vault -- --nocapture --test-threads=1
          cargo test -p monoengine-core 'notification::dispatcher::tests::integration_mail_dispatcher' -- --nocapture
          cargo test -p monoengine-core 'notification::service::tests' -- --nocapture
          cargo test -p monoengine-core 'notification::triggers::tests' -- --nocapture
```

> 上述为示例。仓库实际生效的工作流是 `.github/workflows/config-validation.yml`，其「Check formatting」步骤跑 `cargo +nightly fmt --all --check`、「Lint」步骤跑 `cargo clippy --all-targets --all-features -- -D warnings`（与本示例及 general.md 第 297/310 行强制的 clippy 验收门禁对齐；stable 工具链以 `--component clippy` 安装），「Start test services」步骤以 `docker compose ... up -d --wait` 启动 postgres+redis+mailpit，「Mask test secrets」步骤注入 `::add-mask::`，「Run integration tests」步骤运行上面这组集成测试。

## 数据流与控制流契约

### CLI secret set/check

```text
args
  -> LoadMode::VaultBootstrap
  -> require_config_path
  -> Config::load_vault_bootstrap(path)       # 只解析 database
  -> VaultCore::from_database_config(...)
  -> write_secret/read_secret("secret/<name>")
  -> stdout: SecretRef 或 ok
```

禁止触达：
- Redis
- object storage
- full `Storage::new`
- `AppContext::new`
- SMTP/mail dispatcher
- HTTP/SSH service

### service http

```text
AppContext::new(config):
  db_connection = database_connection(database)        // applies migrations
  -> VaultCore::from_database_connection(db_connection) // DB-only vault bootstrap
        .with_audit_config(...)
  -> resolve_object_storage_secrets(object_storage, vault)  // post-vault SecretRef resolution
  -> object_store = build_object_storage(resolved_object_storage)
  -> Storage::new_with_connection(config, db_connection, object_store)
  -> init_connection(redis)
  -> resolve mail.password_ref (post-vault) -> mailer_from_config
  -> tokio::spawn(NotificationService::start)
  -> mono_service.init_monorepo
  -> HTTP bind
```

测试断言必须与这条顺序一致。特别是：`mail.password_ref` 可以解析，因为 mail 在 Vault 后构造；
数据库凭据在 Vault bootstrap 过程中被消费，不能改为 monoengine Vault SecretRef；**Redis URL 已在
Vault 后接入 `vault://` SecretRef（2026-06-28），合法 namespace 为
`vault://secret/config/<profile>/redis/url#<field>`，且 `config validate`/CLI/`--resolve-secrets`
已对齐**；对象存储凭据已可在启动路径解析 `vault://` SecretRef（DB-only vault bootstrap 后，
`config validate`/CLI 已对齐）。

## 性能与可靠性规则

- 复用一组 Docker 服务；不要每个测试重建容器。
- 每个测试使用独立配置目录、对象存储目录和唯一数据前缀。
- 涉及同一 PostgreSQL 数据库的黑盒测试默认串行运行，或为每个测试创建独立数据库/schema。
- 等待服务就绪必须使用 healthcheck、端口探测或 HTTP 轮询；禁止固定 sleep 作为唯一同步机制。
- 所有外部进程必须有超时和 cleanup。测试失败时保留日志目录，但必须停止子进程。
- Mailpit 轮询应有上限，例如 10 秒内每 200ms 检查一次。
- Dispatcher 测试应控制 pending job 数量，避免一次插入大量任务造成慢测。

## 安全与合规规则

- secret value 只能通过 stdin、临时文件权限受控的文件或 CI secret 注入；禁止命令行参数传明文。
- CI 中使用 `::add-mask::` 或平台等价能力屏蔽测试密码。
- 测试配置中的域名使用 `.test`，不要使用真实生产地址。
- SMTP 测试可使用 `starttls=false`，但只允许指向本机 Mailpit。
- 任何日志、stderr、panic 信息不得包含 DB/Redis URL 密码、SMTP 密码、Vault runtime token、
  root token、secret share。
- 测试生成的 `core_key.json` 必须位于临时目录；测试结束后删除。失败时可保留加密/脱敏日志，
  不保留明文 secret。
- Docker 镜像应固定主版本或具体 tag，避免 `latest` 漂移导致 CI 不稳定。

## 风险与约束

（性能/可靠性与安全/合规的细则见上文「性能与可靠性规则」「安全与合规规则」；本节汇总集成测试的关键风险与约束。）

- **风险：Docker 端口与开发机冲突。**
  - 影响：本地或 CI 测试因端口占用失败。
  - 缓解措施：`docker-compose.test.yml` 统一使用高位 host 端口（15432/16379/11025/18025）。
- **风险：测试栈未启动。**
  - 影响：依赖 PostgreSQL/Redis 的黑盒 gate（如 `bin/tests/integration_vault.rs` 的 service/redaction 用例）在 compose 栈缺失时**硬失败（panic）**，必须先 `docker compose -f docker-compose.test.yml up -d --wait`。
  - 缓解措施：仅 Mailpit 相关用例（如 `integration_mail_dispatcher_mailpit_sends_outbox_job`）在探测 `MAILPIT_API_URL` 不可达时优雅跳过（`eprintln` 提示并 `return`，不 `panic!`）；PostgreSQL/Redis 类 gate 以"先起栈"为前提，CI 的「Start test services」步骤保证依赖就绪。
- **风险：手写 schema 偏离生产。**
  - 影响：测试通过但生产 schema/约束不匹配。
  - 缓解措施：强制经 `src/jupiter/migration/*` 真实 migrations 初始化（硬约束 #2）。
- **约束：嵌入式 Vault，无外部 Vault 服务。**
  - 理由：monoengine 架构不依赖外部 Vault。
  - 影响：使用外部 Vault 会验证错误的系统形态。
- **约束：黑盒测试经真实二进制执行。**
  - 理由：需验证启动顺序、退出码、日志脱敏等真实行为。
  - 影响：mock/`use crate::` 路径无法覆盖这些行为。

## 覆盖矩阵

| 测试用例 | 当前级别 | 配置 | DB/migrations | Redis | Vault | Mail | Notification | CLI | HTTP | 脱敏 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `integration_cli_secret_ref` | P0 | - | - | - | - | - | - | ✓ | - | ✓ |
| `integration_cli_secret_set_check` | P0 | ✓ | ✓ | 禁止触达 | ✓ | - | - | ✓ | - | ✓ |
| `integration_config_validate_resolve_secrets` | P0 | ✓ | ✓ | - | ✓ | ✓ | - | ✓ | - | ✓ |
| `integration_service_http_smoke` | P0 | ✓ | ✓ | ✓ | ✓ | ✓ | - | ✓ | ✓ | ✓ |
| `integration_mail_dispatcher_mailpit` | P0/P1 | ✓ | ✓ | ✓ | ✓ | ✓ | outbox | - | 可选 | 部分；真实 SMTP/Mailpit 正路径与 SMTP transport 失败 retry 已落地 |
| `integration_notification_trigger_to_mail` | P1 已落地（模块集成形态） | - | ✓ | - | - | ✓ | ✓ | - | - | - |
| `integration_error_redaction` | P1 DB/Redis 活门禁已落地，余项分散覆盖 | ✓ | ✓ | ✓ | ✓ | ✓ | - | ✓ | ✓ | ✓ |
| `integration_config_init` | P2 已落地（黑盒） | ✓ | - | - | - | - | - | ✓ | - | ✓ |
| `integration_object_storage_s3_secret_ref` | P2 已落地（黑盒） | ✓ | ✓ | 禁止触达 | ✓ | - | - | ✓ | - | ✓ |
| `integration_config_hot_reload` | **P2 已落地（2026-06-28）** | ✓ | - | - | - | - | - | - | ✓ | ✓ |
| `integration_multichannel_notification` | **P2 已落地（2026-06-28）**：启动真实 `service http`，通过写入 `email_jobs` outbox 触发 dispatcher，验证 email/Mailpit、in-app、Slack、webhook 多渠道扇出 | ✓ | ✓ | ✓ | 视渠道 | ✓ | ✓ | - | ✓ | ✓ |

## 迁移步骤（分阶段）

1. **Phase 0：测试基础设施**
   - 新增 `docker-compose.test.yml`。
   - ✅ 新增测试配置生成 helper（黑盒测试在 `bin/tests/common/mod.rs` 中只生成 TOML，不导入 crate）：`write_bootstrap_config` / `write_full_config`，被 `bin/tests/integration_vault.rs::VaultCliEnv` 复用。
   - 约定临时目录、端口、日志和 cleanup。
2. **Phase 1：P0 CLI gate**
   - `integration_cli_secret_ref`
   - `integration_cli_secret_set_check`
   - `integration_config_validate_resolve_secrets`
3. **Phase 2：P0 service/mail gate**
   - `integration_service_http_smoke`
   - `integration_mail_dispatcher_mailpit`
4. **Phase 3：P1 安全与通知 gate**
   - 先修复 redaction 缺口。
   - 再启用 `integration_error_redaction` 和 notification trigger 到 mail。
5. **Phase 4：P2 未来能力 gate**
   - 多渠道通知与对象存储 SecretRef 已落地并补模块集成测试（见上）；`config init` 专门黑盒进程用例已落地（`integration_config_init_creates_safe_skeleton_and_validates`）；**热加载专门黑盒进程用例 `integration_config_hot_reload` 已落地（2026-06-28）**，基于 base/profile 文件 watcher 触发，验证白名单热生效、restart-required 旧配置继续服务、坏 TOML 拒绝回退；**多渠道通知专门黑盒进程用例 `integration_multichannel_notification` 已落地（2026-06-28）**，启动真实 `service http`，通过写入 `email_jobs` outbox 触发 dispatcher，验证 email/Mailpit、in-app、Slack、webhook 均收到扇出。

## 前置依赖矩阵

集成测试是 monoengine 的测试基础设施，由各模块协同填充。它**不被**任何单一文档阻塞，也**不阻塞**模块实现（除非模块文档明确以某个 P0 gate 通过为前置）。各项工作与其他文档的依赖如下：

| 本文档的工作 | 对其他文档的依赖 | 类型 | 关键同步点 |
|-----------|-------------|-----|----------|
| P0 CLI secret ref/set/check 门禁 | **config.md**（LoadMode、CLI 参数）、**vault.md**（最小 bootstrap、SecretRef 格式） | 协同 | LoadMode 的 VaultBootstrap 模式与 `mail.password` namespace 校验需与 config/vault 一致 |
| P0 `config validate --resolve-secrets` 门禁 | **config.md**（字段/校验）、**vault.md**（SecretRef 解析、脱敏） | 协同 | 校验错误链必须遵循脱敏规则；secret 缺失/错误路径行为一致 |
| P0 service HTTP smoke（启动顺序） | **config.md**、**vault.md**、**orbit.md**（对象存储注入）、**mail.md**（SMTP 初始化） | 协同 | 启动顺序与各模块 fail-closed 策略必须统一 |
| P0/P1 邮件投递与触发器端到端 | **mail.md**（outbox/dispatcher）、**notification.md**（事件类型、触发器、多渠道扇出） | 协同 | outbox schema、dispatcher tick、trigger API 与文档一致；Mailpit 支持 SMTP 故障模拟 |
| P1 脱敏门禁 | **config.md**（`redaction.rs`、日志策略）、**vault.md**（解析失败策略） | 后置 | `redact_db_url`/`redact_redis_url` 须已落地 |
| P2 热加载黑盒 gate | **config.md**（`ConfigHandle`/watcher/订阅者白名单） | 后置 | 热加载逻辑与单测须已就位，明确 SIGHUP/HTTP 触发方式 |
| P2 多渠道/对象存储黑盒 gate | **notification.md**（渠道抽象）、**vault.md**/**orbit.md**（对象存储 SecretRef） | 后置 | 渠道凭据与对象存储 SecretRef 的 validate/CLI 对齐后再补黑盒 gate |
| 覆盖矩阵维护 | **contract.md**（API DTO 路径）、所有模块文档 | 协同 | API/功能变更先更新矩阵，再落测试 |

## 故障排查

### 容器未就绪

```bash
docker compose -f docker-compose.test.yml ps
docker compose -f docker-compose.test.yml logs postgres
docker compose -f docker-compose.test.yml logs redis
docker compose -f docker-compose.test.yml logs mailpit
```

### 未连接到预期 PostgreSQL

```bash
psql 'postgres://mono:mono_test_password@127.0.0.1:15432/monoengine_it' \
  -c "select current_database(), count(*) from seaql_migrations"
```

如果连接失败或数据不在预期数据库中，优先检查：
- PostgreSQL healthcheck 是否通过。
- `database.db_type` 是否为 `postgres`。
- `database.db_url` 是否使用 `127.0.0.1:15432`。
- 测试是否误用了默认 `config/config.toml`。

### 邮件未送达

```bash
curl -fsS http://127.0.0.1:18025/api/v1/messages
psql "$MEGA_DATABASE__DB_URL" -c \
  "select id,status,retry_count,error_message from email_jobs order by id desc limit 10"
```

检查：
- `[mail] enabled = true`
- `smtp_host = "127.0.0.1"`
- `smtp_port = 11025`
- `starttls = false`
- `EmailDispatcher` 是否已随 service 启动

### SecretRef 解析失败

```bash
monoengine --config <temp>/config.toml \
  config secret check mail.password \
  --ref vault://secret/config/it/mail/password#value
```

检查：
- `mail.password` 是否用 `config secret set` 写入。
- `password_ref` 是否缺少 `#field`。
- `vault-path` 是否错误地带了 `secret/` 前缀。
- 测试是否使用了不同的 `MEGA_BASE_DIR`，导致 `core_key.json` 不一致。

## 小结

本集成测试方案用接近生产的 PostgreSQL/Redis/Mailpit 容器栈 + 嵌入式 `VaultCore` + 真实 migrations，按 `Config → database_connection()（含 migrations）→ DB-only Vault bootstrap → 对象存储 → Storage → Redis → Mail/Notification → HTTP` 的真实启动顺序，验证跨模块数据流、控制流与脱敏边界。它把已可落地的 P0/P1 gate（CLI secret、config validate、service smoke、邮件投递与触发器、脱敏门禁）与未来能力 gate（P2 热加载/多渠道/对象存储黑盒）显式分离，前者已在 `bin/tests/` 与 `monoengine-core` 模块集成测试 + CI 中稳定执行。原方案中“外部 Vault + 手写 schema”的偏差已修订为嵌入式 Vault + 真实 migrations，与当前 workspace 架构一致。

## 预期收益

- 使用真实 migrations 和嵌入式 Vault，避免测试通过但生产 schema/启动链路失败。
- 明确 workspace（`monoengine-core` lib + `monoengine` bin）的黑盒测试边界：`bin/tests/` 走进程级 CLI/HTTP，模块集成测试在 `monoengine-core` 内。
- 把当前可落地 gate 与未来能力 gate 分离，让 CI 能先稳定覆盖关键路径。
- 把 DB fallback、secret 泄露、service 启动顺序等高风险问题变成可观测、可诊断的测试目标。
