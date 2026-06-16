# Monoengine 集成测试方案

本文档描述 monoengine 的集成测试策略，基于 Docker 容器化环境，验证配置系统、Vault 安全、邮件通知、多渠道通知等核心模块的端到端功能。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

## 概述

集成测试的目标是验证以下跨模块功能：

1. **配置加载与初始化链路** — `Config::new` → `Storage::new` → `VaultCore::new` → 服务启动
2. **敏感凭据管理** — 数据库密码、Redis URL、邮件密码从文件/环境变量加载，进入 Vault，通过 SecretRef 解析
3. **邮件通知端到端** — 配置 → SMTP mailer 构造 → dispatcher 启动 → 发送邮件
4. **多渠道通知** — 触发器 → NotificationStorage → dispatcher 分发 → 各渠道投递
5. **CLI 两阶段加载** — `config init` → `config secret set` → `config validate --resolve-secrets` → 服务启动
6. **Contract 边界 smoke check** — API DTO、Git protocol、Vault bootstrap、Policy guard 在 `contract::*` 新路径下保持行为不变

## 假设

- **开发机环境**：部署机或 CI 环境已安装 Docker 和 Docker Compose
- **外部服务依赖**：通过容器网络部署 PostgreSQL、Redis、SMTP 服务（如 MailHog）、Vault（可选）
- **配置隔离**：每个集成测试用例使用独立的临时配置目录与数据库/vault 快照
- **可重复性**：测试应能在不同机器上重复运行，不依赖本地特定路径或凭据
- **清理机制**：测试失败时仍应清理容器、临时文件和数据库状态，避免污染后续测试

## 测试环境架构

### 容器编排文件：`docker-compose.test.yml`

```yaml
version: '3.8'

services:
  # PostgreSQL — 元数据存储（配置、vault、通知）
  postgres:
    image: postgres:15-alpine
    environment:
      POSTGRES_USER: mono
      POSTGRES_PASSWORD: mono_test_password
      POSTGRES_DB: monoengine
    ports:
      - "5432:5432"
    healthcheck:
      test: ["CMD", "pg_isready", "-U", "mono"]
      interval: 2s
      timeout: 5s
      retries: 10
    volumes:
      # 挂载初始化脚本，建立必要的表（vault、notifications 等）
      - ./ci/sql/init-test-db.sql:/docker-entrypoint-initdb.d/01-init.sql

  # Redis — 缓存与消息队列
  redis:
    image: redis:7-alpine
    ports:
      - "6379:6379"
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 2s
      timeout: 5s
      retries: 10

  # SMTP 测试服务（MailHog）— 邮件截获与检查
  mailhog:
    image: mailhog/mailhog:latest
    ports:
      - "1025:1025"  # SMTP port
      - "8025:8025"  # Web UI
    healthcheck:
      test: ["CMD", "wget", "--quiet", "--tries=1", "--spider", "http://localhost:8025"]
      interval: 2s
      timeout: 5s
      retries: 10

  # Vault（可选）— 敏感凭据存储
  vault:
    image: vault:1.15-alpine
    environment:
      VAULT_DEV_ROOT_TOKEN_ID: integration-test-root-token
      VAULT_DEV_LISTEN_ADDRESS: "0.0.0.0:8200"
    ports:
      - "8200:8200"
    healthcheck:
      test: ["CMD", "wget", "--quiet", "--tries=1", "--spider", "http://localhost:8200/ui/"]
      interval: 2s
      timeout: 5s
      retries: 10
    cap_add:
      - IPC_LOCK

networks:
  default:
    name: monoengine-test-net
```

### 初始化脚本：`ci/sql/init-test-db.sql`

```sql
-- Vault 表
CREATE TABLE IF NOT EXISTS vault_core (
    id BIGSERIAL PRIMARY KEY,
    root_token_hash VARCHAR(255),
    seal_status VARCHAR(50) DEFAULT 'Sealed',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Notification 实体（callisto schema 的子集）
CREATE TABLE IF NOT EXISTS user_notification_preferences (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    email_enabled BOOLEAN DEFAULT TRUE,
    in_app_enabled BOOLEAN DEFAULT TRUE,
    slack_enabled BOOLEAN DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS user_notification_settings (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    channel VARCHAR(50) NOT NULL,
    setting_key VARCHAR(100) NOT NULL,
    setting_value VARCHAR(255),
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, channel, setting_key)
);

CREATE TABLE IF NOT EXISTS notification_events (
    id BIGSERIAL PRIMARY KEY,
    event_type VARCHAR(100) NOT NULL,
    user_id BIGINT NOT NULL,
    payload JSONB,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS email_jobs (
    id BIGSERIAL PRIMARY KEY,
    recipient VARCHAR(255) NOT NULL,
    subject VARCHAR(255),
    body TEXT,
    status VARCHAR(50) DEFAULT 'pending',
    retry_count INT DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    sent_at TIMESTAMP
);

-- 索引
CREATE INDEX idx_email_jobs_status ON email_jobs(status);
CREATE INDEX idx_email_jobs_created ON email_jobs(created_at);
```

## 测试场景与验收标准

### 1. 配置初始化与加载（`test_config_init_and_load`）

**步骤**：
1. 使用 `monoengine config init` 在临时目录生成配置骨架
2. 验证生成的 `config/config.toml` 包含必要字段（数据库、Redis、邮件）
3. 验证生成的配置不包含硬编码密码或可复用凭据
4. 运行 `monoengine config validate` 检查配置合法性

**验收标准**：
- ✅ 配置文件成功生成
- ✅ TOML 语法合法，可被反序列化
- ✅ 所有必填字段都有占位符或部署指引
- ✅ 不包含硬编码密码（如 `postgres://mega:mega@...`）
- ✅ 校验输出正确列举所有错误（缺少数据库 URL、Redis URL 等）

### 2. 数据库连接与初始化（`test_database_bootstrap`）

**步骤**：
1. 通过环境变量 `MEGA_DATABASE__DB_URL` 注入 PostgreSQL 连接
2. 启动 monoengine，验证 `Storage::new` 成功连接数据库
3. 验证必要的表（vault_core、email_jobs、user_notification_* 等）已创建
4. 检查运行时操作（如查询用户偏好）能正常进行

**验收标准**：
- ✅ 数据库连接成功
- ✅ 初始化脚本执行完毕，表结构正确
- ✅ 查询和 DML 操作正常
- ✅ 连接失败时输出可诊断错误（不 panic）

### 3. Vault 初始化与 Secret 存储（`test_vault_bootstrap_and_secrets`）

**步骤**：
1. 启动 Vault 容器（开发模式）
2. 运行 `monoengine config secret set mail.password --vault-path config/test/mail/password --field value --value-stdin`
3. 验证 secret 成功写入 Vault
4. 运行 `monoengine config secret check` 验证 secret 可读
5. 验证 resolver 缓存与 evict 机制

**验收标准**：
- ✅ Secret 写入 Vault 成功（无明文输出）
- ✅ Secret check 能检验 secret 存在、权限合法
- ✅ 缓存命中率能被观测（日志、指标）
- ✅ Evict 接口清除缓存后，下一次读取重新从 Vault 获取

### 4. SMTP Mailer 与邮件发送（`test_mail_dispatcher`）

**步骤**：
1. 配置 SMTP 指向 MailHog（端口 1025）
2. 启动 monoengine，验证 `SmtpMailer::new` 成功，`EmailDispatcher` spawn
3. 手动插入 email_jobs 记录或触发通知事件
4. 通过 MailHog 的 HTTP API（端口 8025）检查邮件是否被接收
5. 验证 dispatcher 的 retry 机制（连接失败时重试，成功后标记 sent_at）

**验收标准**：
- ✅ Mailer 启动后不 panic，dispatcher 正常运行
- ✅ 邮件能被 MailHog 接收（至少一个邮件出现在 `/api/v1/messages`）
- ✅ Dispatcher 的 retry 逻辑生效（失败重试、成功更新状态）
- ✅ 邮件失败时日志脱敏（不输出密码）

### 5. 通知触发器与多渠道分发（`test_notification_triggers`）

**步骤**：
1. 创建用户记录并设置 notification preferences（启用邮件）
2. 通过 API 触发通知事件（如评论创建）
3. 验证 NotificationStorage 创建对应 email_job
4. 验证 dispatcher 分发并通过邮件渠道投递
5. 验证禁用渠道（如 Slack）的事件被跳过

**验收标准**：
- ✅ 事件触发后出现 email_job 记录
- ✅ 邮件成功发送给用户
- ✅ 已禁用的渠道不产生任何投递记录
- ✅ 错误处理：缺少用户偏好时给出诊断错误（不 panic）

### 6. CLI 完整工作流（`test_cli_workflow_complete`）

**步骤**：
1. 在裸机上执行 `config init`（无数据库，无 vault）
2. 配置 `.env` 注入数据库凭据和 Redis URL
3. 执行 `config validate`（只需配置文件合法）
4. 启动 PostgreSQL 和 Vault 容器
5. 执行 `config secret set mail.password --value-stdin`
6. 执行 `config validate --resolve-secrets`（需要 vault 就绪）
7. 启动服务 `monoengine service http`
8. 验证服务绑定端口、接收请求

**验收标准**：
- ✅ `config init` 在无依赖时成功生成配置
- ✅ `config validate` 报告具体的配置错误（不 panic）
- ✅ `config secret set` 正确写入 Vault，不在日志中泄露密码
- ✅ `config validate --resolve-secrets` 验证 SecretRef 可解析
- ✅ 服务成功启动，能处理请求

### 7. 配置热加载与白名单（`test_config_hot_reload`）

**步骤**：
1. 启动服务
2. 修改配置文件中的白名单字段（如日志级别）
3. 发送 SIGHUP 或调用热加载 API
4. 验证新配置生效，不需要重启
5. 修改非白名单字段（如数据库 URL）
6. 验证告警或拒绝加载，旧配置继续生效

**验收标准**：
- ✅ 白名单字段变更后立即生效
- ✅ 非白名单字段变更被拒绝或告警
- ✅ 热加载失败时旧配置继续生效，进程不中断
- ✅ 日志记录变更源、字段路径和结果

### 8. 错误诊断与脱敏（`test_error_diagnosis_and_redaction`）

**步骤**：
1. 提供坏 TOML 配置，验证错误信息包含文件路径和字段路径
2. 配置错误的数据库 URL，验证连接错误被脱敏（不泄露密码）
3. 配置错误的 SMTP 密码，验证邮件发送失败不在日志中输出明文
4. 缺少 vault root token，验证启动失败给出 fail-closed 错误

**验收标准**：
- ✅ 配置错误输出包含文件路径、行号、字段路径
- ✅ 日志和错误信息不包含敏感值（使用 `[REDACTED]` 或 `***`）
- ✅ 错误信息给出修复建议
- ✅ Vault 缺失时明确提示初始化步骤，不自动清空数据

## 测试执行

### 快速本地测试

```bash
# 启动测试环境
docker-compose -f docker-compose.test.yml up -d

# 等待所有服务就绪
docker-compose -f docker-compose.test.yml exec postgres pg_isready -U mono
docker-compose -f docker-compose.test.yml exec redis redis-cli ping

# 构建 monoengine
cargo build --release

# 执行集成测试
source .env.test
cargo test --test integration_tests -- --nocapture

# 清理
docker-compose -f docker-compose.test.yml down -v
```

### CI/CD 集成（GitHub Actions 示例）

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
          POSTGRES_DB: monoengine
        options: >-
          --health-cmd pg_isready
          --health-interval 2s
          --health-timeout 5s
          --health-retries 10
        ports:
          - 5432:5432
      
      redis:
        image: redis:7-alpine
        options: >-
          --health-cmd "redis-cli ping"
          --health-interval 2s
          --health-timeout 5s
          --health-retries 10
        ports:
          - 6379:6379
      
      mailhog:
        image: mailhog/mailhog:latest
        ports:
          - 1025:1025
          - 8025:8025

    steps:
      - uses: actions/checkout@v3
      
      - uses: dtolnay/rust-toolchain@stable
      
      - name: Run integration tests
        env:
          MEGA_DATABASE__DB_URL: postgres://mono:mono_test_password@localhost:5432/monoengine
          MEGA_REDIS__URL: redis://localhost:6379
          MEGA_MAIL__ENABLED: "true"
          MEGA_MAIL__SMTP_HOST: localhost
          MEGA_MAIL__SMTP_PORT: "1025"
          MEGA_MAIL__USERNAME: test@example.com
          MEGA_MAIL__PASSWORD: test_password
        run: |
          cargo test --test integration_tests --release -- --nocapture
```

## 测试覆盖矩阵

| 测试用例 | Contract | 配置 | Vault | 邮件 | 通知 | CLI | 热加载 | 脱敏 |
|---------|----------|------|-------|------|------|-----|-------|------|
| `test_config_init_and_load` | - | ✓ | - | - | - | ✓ | - | ✓ |
| `test_database_bootstrap` | - | ✓ | - | - | - | - | - | - |
| `test_vault_bootstrap_and_secrets` | ✓ | ✓ | ✓ | - | - | ✓ | - | ✓ |
| `test_mail_dispatcher` | - | ✓ | ✓ | ✓ | - | - | - | ✓ |
| `test_notification_triggers` | - | ✓ | ✓ | ✓ | ✓ | - | - | - |
| `test_cli_workflow_complete` | ✓ | ✓ | ✓ | ✓ | - | ✓ | - | ✓ |
| `test_config_hot_reload` | - | ✓ | - | - | - | - | ✓ | - |
| `test_error_diagnosis_and_redaction` | - | ✓ | ✓ | ✓ | - | ✓ | - | ✓ |

## 集成测试与改进计划的对应

集成测试的执行顺序与文档中的阶段规划对应：

| 计划阶段 | 集成测试覆盖 | 备注 |
|---------|-----------|------|
| config 0a/0b/1/2 | `test_config_init_and_load`、`test_error_diagnosis_and_redaction` | 配置初始化、验证、CLI 命令 |
| contract 0-2 | `cargo check`、`test_vault_bootstrap_and_secrets`、Git/API smoke tests | 新模块路径下行为不变 |
| config 3、vault B | `test_database_bootstrap` | DB bootstrap、fail-closed 行为 |
| config 4、vault E | `test_vault_bootstrap_and_secrets`、`test_cli_workflow_complete` | Secret 读写、resolver、命令完整流程 |
| config 5、mail 2 | `test_mail_dispatcher` | SecretRef、mailer 构造、dispatcher 启动 |
| mail 3+、notification 1-3 | `test_notification_triggers` | 触发器、多渠道分发、偏好管理 |
| config 8 | `test_config_hot_reload` | 热加载白名单、失败回滚 |

## 与现有测试的关系

- **单元测试**（`src/**/*.rs` 中的 `#[test]`）：测试单个函数和模块，不依赖外部服务
- **加载测试**（`tests/load_*.rs`）：测试配置文件加载、占位符展开、反序列化，使用临时文件
- **集成测试**（`tests/integration_*.rs`）：本文档定义的端到端测试，依赖 Docker 容器服务
- **CI 配置校验**：GitHub Actions 验证基础样例配置、默认模板、生成结果

## 故障排查

### 容器启动失败

```bash
# 检查容器日志
docker-compose -f docker-compose.test.yml logs postgres
docker-compose -f docker-compose.test.yml logs redis
docker-compose -f docker-compose.test.yml logs mailhog

# 检查容器是否在运行
docker-compose -f docker-compose.test.yml ps

# 强制重建容器
docker-compose -f docker-compose.test.yml down -v
docker-compose -f docker-compose.test.yml up -d
```

### 邮件未送达

```bash
# 检查 MailHog API
curl http://localhost:8025/api/v1/messages

# 检查 SMTP 连接
telnet localhost 1025

# 检查 monoengine 日志中的脱敏输出
grep -i "mail\|smtp\|redacted" monoengine.log
```

### 数据库连接失败

```bash
# 验证 PostgreSQL 健康
docker-compose -f docker-compose.test.yml exec postgres pg_isready -U mono

# 检查连接字符串
echo $MEGA_DATABASE__DB_URL

# 手动测试连接
psql $MEGA_DATABASE__DB_URL -c "SELECT 1"
```

## 实施路线图

1. **Phase 1**（第 1 阶段）：建立基础环境（docker-compose.test.yml、初始化脚本）
2. **Phase 2**（第 2 阶段）：实现配置与数据库测试（test_config_init_and_load、test_database_bootstrap）
3. **Phase 3**（第 3 阶段）：实现 Vault 与 CLI 测试（test_vault_bootstrap_and_secrets、test_cli_workflow_complete）
4. **Phase 4**（第 4 阶段）：实现邮件与通知测试（test_mail_dispatcher、test_notification_triggers）
5. **Phase 5**（第 5 阶段）：实现高级测试（test_config_hot_reload、test_error_diagnosis_and_redaction）
6. **Phase 6**（第 6 阶段）：CI/CD 集成与报告

## 预期收益

- **跨模块验证**：确保配置 → Vault → 邮件 → 通知 的完整链路正常
- **端到端可靠性**：发现单元测试遗漏的集成问题（如 secret 缓存与轮换）
- **部署信心**：在真实 Docker 环境下验证启动流程和故障恢复
- **文档同步**：测试场景与改进计划的各个阶段对应，确保文档不脱节
- **CI/CD 闭环**：自动化验证配置、CLI、secret、邮件等关键路径
