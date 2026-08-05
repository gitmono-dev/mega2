# 本地开发与测试指南

按本文步骤可跑通**普通测试**与**完整集成测试（IT）**。策略与服务登记细节见
[`refactoring/integration.md`](./refactoring/integration.md)、
[`refactoring/test-infra.md`](./refactoring/test-infra.md)；提交前三门禁见
[`AGENTS.md`](../AGENTS.md)。

**MonoRepo 产品规则**（公开分支仅 `main`、禁止 Git 客户端操作 tag、初始化与目录结构）见集中文档
[`monorepo.md`](./monorepo.md)。

## 推荐：用脚本代替手贴命令

仓库根提供统一入口 [`scripts/dev-test.sh`](../scripts/dev-test.sh)，避免复制粘贴漏步骤
（尤其是 `git-cli` / `.env.test` / UID）。共享逻辑在
[`scripts/lib/monoengine-it.sh`](../scripts/lib/monoengine-it.sh)。

```bash
./scripts/dev-test.sh --help

# 栈
./scripts/dev-test.sh up-data          # 仅数据面
./scripts/dev-test.sh up-full          # 数据面 + git-cli（Linux，推荐）
./scripts/dev-test.sh health
./scripts/dev-test.sh down

# 测试
./scripts/dev-test.sh unit             # 无 compose 的 lib 单测
./scripts/dev-test.sh basic            # 数据面 + cargo test --all（无 git-cli）
./scripts/dev-test.sh full             # 完整 IT（推荐路径）
./scripts/dev-test.sh vault            # integration_vault
./scripts/dev-test.sh git-cli          # integration_git_cli
./scripts/dev-test.sh gates            # fmt + clippy + full IT
```

向后兼容的手贴步骤见下文各节；**新流程优先用脚本**。

## 概念（先读这四条）

1. **Compose = 数据面**：`docker-compose.test.yml` 提供 Postgres / Redis / RustFS 等
   monoengine 依赖；Mailpit 仅供 website 的认证/产品邮件 IT 捕获，**不**替代用例内拉起的被测进程。
2. **黑盒隔离**：`bin/tests/integration_*.rs` 通过 `CARGO_BIN_EXE_monoengine`
   按用例启动独立 `service http`（独立端口、临时目录、隔离 DB）。
3. **`--profile app` ≠ 隔离 IT**：compose 常驻 `monoengine`（`:19180`）只做栈级
   smoke / 手工探针；**不**替代黑盒 per-case 隔离。
4. **项目名固定**：凡启停命令一律带 **`-p monoengine-it`**（与默认目录名项目不可并存）。

## 前置条件

| 项 | 说明 |
| --- | --- |
| OS | **全量 IT / `git-cli` 验收目标为 Linux**（host 网络）；非 Linux 可跑数据面 + 不含 git-cli 的子集 |
| 工具 | Docker Compose v2、Rust stable、nightly（仅 `rustfmt` 门禁） |
| 仓库布局 | 源码构建 compose `monoengine` 时需要 sibling `../orbit` |
| 配置 | 从示例生成本地 env（不提交）：`cp .env.test.example .env.test`（`dev-test.sh` 会自动创建） |

公开测试凭据（仅 IT 栈，已写在 compose / example 中）：

- Postgres：用户/库 `monoengine`，密码 `monoengine_test_password`
- RustFS：`rustfs` / `rustfs_secret`，桶 `testbucket`

## 快速开始：普通 / 基础测试

无外部依赖的单测可直接跑（不必起 compose）：

```bash
./scripts/dev-test.sh unit
# 或按子串过滤
./scripts/dev-test.sh unit <substring> -- --nocapture
# 等价手贴：cargo test -p monoengine-core --lib
```

需要 DB / Redis 的 crate 内集成与多数黑盒用例：先起**默认数据面**，再注入 env：

```bash
./scripts/dev-test.sh basic
# 等价手贴：
# docker compose -p monoengine-it -f docker-compose.test.yml up -d --wait
# cp -n .env.test.example .env.test && source .env.test
# cargo test --all
```

健康自检：

```bash
./scripts/dev-test.sh health
```

## 完整集成测试栈（推荐路径）

在仓库根执行。目标：**数据面 + RustFS 桶初始化 + Linux `git-cli`**，然后跑全量测试。

```bash
./scripts/dev-test.sh full
```

等价手贴：

```bash
# 1) 共享 git 工作根（必须先 mkdir，避免 Docker 以 root 建目录导致 EACCES）
dir="${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-git}"
mkdir -p "$dir" && chmod 1777 "$dir"
export MONOENGINE_IT_GIT_UID="$(id -u)" MONOENGINE_IT_GIT_GID="$(id -g)"

# 2) 数据面 + rustfs-init 建桶 + git-cli（Linux；mailpit 仅供 website IT 捕获）
#    一次 --profile git up，避免漏启 git-cli 导致 integration_git_cli 硬失败。
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile git up -d --wait

# 3) 注入连接串并跑全量（含黑盒 + 模块集成）
cp -n .env.test.example .env.test
# 若要跑 RustFS / S3-compatible smoke，取消 .env.test 中 MEGA_OBJECT_STORAGE__* 注释后重新 source
source .env.test
cargo test --all
```

仅数据面、不跑 `integration_git_cli` 时，可省略 `--profile git`：

```bash
./scripts/dev-test.sh up-data
# 或：docker compose -p monoengine-it -f docker-compose.test.yml up -d --wait
```

用完清理（含 profile 服务与命名卷）：

```bash
./scripts/dev-test.sh down
# 等价：
# docker compose -p monoengine-it -f docker-compose.test.yml \
#   --profile git --profile app --profile web down -v
```

## Compose profiles

| Profile | 服务 | 何时启用 |
| --- | --- | --- |
| （默认） | `postgres`、`redis`、`rustfs`、`rustfs-init` | monoengine 日常 IT 数据面；`rustfs-init` 幂等建 `testbucket` 后常驻供 `--wait` |
| （默认，可选消费） | `mailpit` | website 认证/产品邮件捕获；不是 monoengine 测试门 |
| `git` | `git-cli`（host 网络，Linux） | 跑 `integration_git_cli` / `cargo test --all` 全量门 |
| `app` | 常驻 `monoengine` → `127.0.0.1:19180` | 栈级 HTTP smoke / 联调；**不是**隔离黑盒 |

栈级 HTTP 探针示例（需先 build 镜像，见 `test-infra.md`）：

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app up -d --wait monoengine
curl -sf http://127.0.0.1:19180/api/openapi.json >/dev/null
# 可选：export MONOENGINE_IT_HTTP_URL=http://127.0.0.1:19180
# cargo test -p monoengine --test integration_vault integration_compose_monoengine_http_smoke
```

## 聚焦命令

```bash
./scripts/dev-test.sh vault
./scripts/dev-test.sh git-cli
# 等价手贴：
# source .env.test
# cargo test -p monoengine --test integration_vault -- --nocapture --test-threads=1
# cargo test -p monoengine --test integration_git_cli -- --nocapture --test-threads=1
```

提交前三门禁（与 `AGENTS.md` 一致）：

```bash
./scripts/dev-test.sh gates
# 等价手贴：
# cargo +nightly fmt --all --check
# cargo clippy --all-targets --all-features -- -D warnings
# source .env.test && cargo test --all   # 需已 up-full；gates 会自行 up-full
```

## 端口与凭据一览

| 服务 | Host 绑定 | 用途 |
| --- | --- | --- |
| postgres | `127.0.0.1:15432` → 5432 | `MEGA_DATABASE__DB_URL` |
| redis | `127.0.0.1:16379` → 6379 | `MEGA_REDIS__URL` |
| mailpit SMTP | `127.0.0.1:11025` → 1025 | website IT SMTP 捕获（非 monoengine） |
| mailpit UI/API | `127.0.0.1:18025` → 8025 | website IT 捕获查看（非 monoengine） |
| rustfs S3 API | `127.0.0.1:19000` → 9000 | `MEGA_OBJECT_STORAGE__S3__ENDPOINT_URL` |
| rustfs console | `127.0.0.1:19001` → 9001 | 人工查看 |
| monoengine（`app`） | `127.0.0.1:19180` → 8000 | `MONOENGINE_IT_HTTP_URL` |
| git-cli | 无端口映射 | host 网络访问宿主高位端口 |

网络名固定为 `monoengine-test-network`：带 `-p monoengine-it` 与不带 `-p` 的两套栈会争用，启新栈前先 `down -v` 旧栈。

## 环境变量：资源连接 vs harness

**资源连接**（`.env.test` / `.env.test.example`，`export` 后被进程读取）：

- `MEGA_DATABASE__DB_TYPE` / `MEGA_DATABASE__DB_URL`
- `MEGA_REDIS__URL`
- `MAILPIT_API_URL` / `MAILPIT_SMTP_HOST` / `MAILPIT_SMTP_PORT`：仅 website
  认证/产品邮件 IT 捕获；monoengine 不读取它们
- `MEGA_NOTIFICATION__WEBSITE_MAIL_BASE_URL` /
  `MEGA_NOTIFICATION__WEBSITE_MAIL_BEARER`：isolated IT 的 website 产品邮件
  API 客户端占位；不是 SMTP 配置
- 可选 S3：`MEGA_OBJECT_STORAGE__STORAGE_TYPE=s3compatible` 及 `MEGA_OBJECT_STORAGE__S3__*`
  （须与 compose 中 `rustfs` / `rustfs_secret` 一致）

**Harness / compose 编排**（`MONOENGINE_IT_*`，控制 runner 而非业务配置字段名）：

| 变量 | 作用 |
| --- | --- |
| `MONOENGINE_IT_GIT_WORKDIR` | git-cli 共享宿主根（默认 `/tmp/monoengine-git`）；变更后须 `--force-recreate git-cli` |
| `MONOENGINE_IT_GIT_UID` / `GID` | 容器内用户；本地 `id -u` ≠ 1000 时必设（脚本默认导出当前用户） |
| `MONOENGINE_IT_HTTP_URL` | 指向 compose `app` 常驻服务 |
| `MONOENGINE_IT_ALLOW_HOST_GIT=1` | **仅** Linux 本地实验用宿主机 git；**不是**验收路径 |
| `MONOENGINE_IT_SKIP_GIT_CLI=1` | 显式跳过 git-cli 用例（非默认门禁） |
| `MONOENGINE_IT_PROJECT` | Compose 项目名（默认 `monoengine-it`；脚本可覆盖） |

不要把 compose **服务名**（如 `postgres`）写进宿主侧 URL——宿主进程一律连 `127.0.0.1:<高位端口>`；容器内（profile `app`）才用服务 DNS 名。

## 故障排查与重置

**栈未就绪 / 连错库**

```bash
docker compose -p monoengine-it -f docker-compose.test.yml ps
docker compose -p monoengine-it -f docker-compose.test.yml logs postgres redis mailpit rustfs
psql 'postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine' \
  -c "select current_database(), count(*) from seaql_migrations"
```

确认 `source .env.test` 后 URL 指向 `15432` / `16379`，且未误用生产 `config/config.toml` 默认值。

**`git-cli runner unavailable`**

- 是否执行了 `./scripts/dev-test.sh up-full`（或 `--profile git up -d --wait`）？
- 工作根是否已 `mkdir` + `chmod 1777`？UID/GID 是否与宿主一致？
- 宿主机 `git` **不能**替代验收；仅调试时可设 `MONOENGINE_IT_ALLOW_HOST_GIT=1`。

**git 工作目录 EACCES / 挂载分叉**

```bash
./scripts/dev-test.sh up-full
# 若仍异常，强制重建 git-cli：
dir="${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-git}"
mkdir -p "$dir" && chmod 1777 "$dir"
export MONOENGINE_IT_GIT_UID="$(id -u)" MONOENGINE_IT_GIT_GID="$(id -g)"
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile git up -d --force-recreate --wait git-cli
```

**website 邮件未进 Mailpit**

```bash
curl -fsS http://127.0.0.1:18025/api/v1/messages
```

检查 website-next 的测试邮件 provider / SMTP 配置；monoengine 没有 SMTP 配置，也不以
Mailpit 可用性作为启动或测试门。

**干净重置**

```bash
./scripts/dev-test.sh down
# 刷新本地 env（保留自定义注释时请手动合并）
cp .env.test.example .env.test
```

零残留可按 compose project label 检查卷与网络为空（见 `test-infra.md`）。

## 相关文档

- 编排事实源：[`docker-compose.test.yml`](../docker-compose.test.yml)
- Env 模板：[`.env.test.example`](../.env.test.example)
- 测试脚本：[`scripts/dev-test.sh`](../scripts/dev-test.sh)
- 基建规范 / 服务登记：[`docs/refactoring/test-infra.md`](./refactoring/test-infra.md)
- IT 策略与覆盖矩阵：[`docs/refactoring/integration.md`](./refactoring/integration.md)
- Agent 提交门禁：[`AGENTS.md`](../AGENTS.md)
