# 本地开发与测试指南

按本文步骤可跑通**普通测试**与**完整集成测试（IT）**。策略与服务登记细节见
[`refactoring/integration.md`](./refactoring/integration.md)、
[`refactoring/test-infra.md`](./refactoring/test-infra.md)；提交前三门禁见
[`AGENTS.md`](../AGENTS.md)。

**Monorepo 产品规则**（公开分支仅 `main`、禁止 Git 客户端操作 tag、初始化与目录结构、trunk 不变式）见集中文档
[`monorepo.md`](./monorepo.md)。Trunk / storage-only 部署见 [`deploy-trunk.md`](./deploy-trunk.md)。

## 推荐：用脚本代替手贴命令

仓库根提供统一入口 [`scripts/dev-test.sh`](../scripts/dev-test.sh)，避免复制粘贴漏步骤
（尤其是 `git-cli` / `.env.test` / UID）。共享逻辑在
[`scripts/lib/mega2-it.sh`](../scripts/lib/mega2-it.sh)。

```bash
./scripts/dev-test.sh --help

# 栈
./scripts/dev-test.sh up-data          # 仅数据面
./scripts/dev-test.sh up-full          # 数据面 + git-cli（推荐）
./scripts/dev-test.sh up-scorpio       # 数据面 + mega2 + scorpiofs（ScorpioFS 联调）
./scripts/dev-test.sh health
./scripts/dev-test.sh down

# 测试
./scripts/dev-test.sh unit             # 无 compose 的 lib 单测
./scripts/dev-test.sh basic            # 数据面 + cargo test --all（无 git-cli）
./scripts/dev-test.sh full             # 完整 IT（推荐路径）
./scripts/dev-test.sh vault            # integration_vault
./scripts/dev-test.sh git-cli          # integration_git_cli
./scripts/dev-test.sh scorpio-smoke    # ScorpioFS 栈级 smoke（需先 up-scorpio）
./scripts/dev-test.sh gates            # fmt + clippy + full IT
```

向后兼容的手贴步骤见下文各节；**新流程优先用脚本**。

## 概念（先读这四条）

1. **Compose = 数据面**：`docker/docker-compose.test.yml` 提供 Postgres / Redis / RustFS 等
   mega2 依赖；Mailpit 仅供 website 的认证/产品邮件 IT 捕获，**不**替代用例内拉起的被测进程。
2. **黑盒隔离**：`tests/integration_*.rs` 通过 `CARGO_BIN_EXE_mega2`
   按用例启动独立 `service http`（独立端口、临时目录、隔离 DB）。
3. **`--profile app` ≠ 隔离 IT**：compose 常驻 `mega2`（`:19180`）只做栈级
   smoke / 手工探针；**不**替代黑盒 per-case 隔离。
4. **项目名固定**：凡启停命令一律带 **`-p mega2-it`**（与默认目录名项目不可并存）。

## 前置条件

| 项 | 说明 |
| --- | --- |
| OS | **全量 IT / `git-cli`** 在 **Linux 与 macOS Docker Desktop** 上验收（bridge + `host.docker.internal`）；Windows 未验收 |
| 工具 | Docker Compose v2、Rust stable、nightly（仅 `rustfmt` 门禁） |
| 仓库布局 | 对象存储内联于 `src/orbit_api/` + `src/orbit/`；compose 构建 megaui 时仍需 sibling `../megaui` |
| 配置 | 从示例生成本地 env（不提交）：`cp .env.test.example .env.test`（`dev-test.sh` 会自动创建） |

公开测试凭据（仅 IT 栈，已写在 compose / example 中）：

- Postgres：用户/库 `mega2`，密码 `mega2_test_password`
- RustFS：`rustfs` / `rustfs_secret`，桶 **`mega2`**（mega2 IT）与保留名称 **`monoui`**（megaui 上传，FS-ME-01）

## 快速开始：普通 / 基础测试

无外部依赖的单测可直接跑（不必起 compose）：

```bash
./scripts/dev-test.sh unit
# 或按子串过滤
./scripts/dev-test.sh unit <substring> -- --nocapture
# 等价手贴：cargo test -p mega2 --lib
```

需要 DB / Redis 的 crate 内集成与多数黑盒用例：先起**默认数据面**，再注入 env：

```bash
./scripts/dev-test.sh basic
# 等价手贴：
# docker compose -p mega2-it -f docker/docker-compose.test.yml up -d --wait
# cp -n .env.test.example .env.test && source .env.test
# cargo test --all
```

健康自检：

```bash
./scripts/dev-test.sh health
```

## 完整集成测试栈（推荐路径）

在仓库根执行。目标：**数据面 + RustFS 桶初始化 + git-cli**，然后跑全量测试。

```bash
./scripts/dev-test.sh full
```

等价手贴：

```bash
# 1) 共享 git 工作根（必须先 mkdir，避免 Docker 以 root 建目录导致 EACCES）
dir="${MEGA2_IT_GIT_WORKDIR:-/tmp/mega2-git}"
mkdir -p "$dir" && chmod 1777 "$dir"
export MEGA2_IT_GIT_UID="$(id -u)" MEGA2_IT_GIT_GID="$(id -g)"

# 2) 数据面 + rustfs-init 建桶 + git-cli（mailpit 仅供 website IT 捕获）
#    一次 --profile git up，避免漏启 git-cli 导致 integration_git_cli 硬失败。
docker compose -p mega2-it -f docker/docker-compose.test.yml \
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
# 或：docker compose -p mega2-it -f docker/docker-compose.test.yml up -d --wait
```

用完清理（含 profile 服务与命名卷）：

```bash
./scripts/dev-test.sh down
# 等价：
# docker compose -p mega2-it -f docker/docker-compose.test.yml \
#   --profile git --profile app --profile web --profile smoke --profile scorpio down -v
```

## Compose profiles

| Profile | 服务 | 何时启用 |
| --- | --- | --- |
| （默认） | `postgres`、`redis`、`rustfs`、`rustfs-init` | mega2 日常 IT 数据面；`rustfs-init` 幂等建 **`mega2`** + **`monoui`** 桶后常驻供 `--wait` |
| （默认，可选消费） | `mailpit` | website 认证/产品邮件捕获；不是 mega2 测试门 |
| `git` | `git-cli`（bridge + `host.docker.internal`） | 跑 `integration_git_cli` / `cargo test --all` 全量门 |
| `app` | 常驻 `mega2` → `127.0.0.1:19180` | 栈级 HTTP smoke / 联调；**不是**隔离黑盒 |
| `scorpio` | `scorpiofs` → `127.0.0.1:12725`（FUSE 守护进程，`depends_on: mega2`） | ScorpioFS ↔ mega2 栈级联调；须与 `--profile app` 同启，见下文「ScorpioFS 联调」 |

栈级 HTTP 探针示例（需先 build 镜像，见 `test-infra.md`）：

```bash
docker compose -p mega2-it -f docker/docker-compose.test.yml \
  --profile app up -d --wait mega2
curl -sf http://127.0.0.1:19180/api/openapi.json >/dev/null
# 可选：export MEGA2_IT_HTTP_URL=http://127.0.0.1:19180
# cargo test -p mega2 --test integration_vault integration_compose_mega2_http_smoke
```

## ScorpioFS 联调

[ScorpioFS](https://github.com/gitmono-dev/scorpiofs) 是把 monorepo 路径挂载成本地文件系统的
FUSE 守护进程，只读路径走 mega2 的 `/api/v1/tree*`、`/api/v1/file/tree`、
`/api/v1/file/blob/{oid}`。`docker/docker-compose.test.yml` 以 profile `scorpio` 提供常驻
`scorpiofs` 服务（登记条目见 [`refactoring/test-infra.md`](./refactoring/test-infra.md)），
**从 sibling checkout `../scorpiofs` 构建** `scorpiofs:local`，并 `depends_on` 常驻
`mega2`（profile `app`），因此两个 profile 必须同启。

前置：`../scorpiofs` 已 checkout；宿主是 rootful Docker 且有 `/dev/fuse`
（容器需要 `--device /dev/fuse` + `CAP_SYS_ADMIN`，另加 `CAP_DAC_READ_SEARCH` 让
passthrough 层的 `open_by_handle_at` 走真实路径而不是回退；rootless Docker 不支持）。

```bash
./scripts/dev-test.sh up-scorpio          # 首次会构建 scorpiofs:local（数分钟）
./scripts/dev-test.sh up-scorpio --build  # ../scorpiofs 改动后强制重建
./scripts/dev-test.sh scorpio-smoke       # 栈级 smoke，见下
ls /tmp/mega2-scorpiofs/mount             # 宿主上直接浏览 monorepo
./scripts/dev-test.sh down                # 连同 scorpio profile 一起 down -v
```

等价手贴：

```bash
dir="${MEGA2_IT_SCORPIO_WORKDIR:-/tmp/mega2-scorpiofs}"
mkdir -p "$dir/mount" "$dir/antares"      # 由测试 UID 创建；mount 必须为空
docker compose -p mega2-it -f docker/docker-compose.test.yml \
  --profile app --profile scorpio up -d --wait
bash scripts/scorpiofs_smoke.sh
```

**挂载对宿主可见。** `scorpiofs` 把宿主 `${MEGA2_IT_SCORPIO_WORKDIR:-/tmp/mega2-scorpiofs}/mount`
与 `…/antares` 以 `rshared` bind 挂到容器 `/mnt/scorpiofs/mount` / `/mnt/scorpiofs/antares`
（经 `SCORPIO_WORKSPACE` / `SCORPIO_ANTARES_MOUNT_ROOT` 覆盖镜像默认路径），容器内做的
FUSE 挂载会传播回宿主：`<workdir>/mount` 就是 monorepo 只读根，Antares 任务挂载出现在
`<workdir>/antares/<mount_id>`。ScorpioFS 以 `allow_other` 挂载，宿主非 root 用户可直接读
（文件属主显示为 root）。前提是宿主路径位于 `shared` 传播的挂载上（systemd 宿主默认）且
dockerd 与宿主共享 mount namespace；macOS Docker Desktop 的传播止于 VM，宿主看不到。

`scripts/scorpiofs_smoke.sh` 用宿主 `curl`（`127.0.0.1:12725`）、宿主 `findmnt` 与
`docker compose … exec -T scorpiofs` 覆盖：`GET /health`；dicfuse 只读根列出初始化树
（`project`、`third-party`）且 `project/.gitkeep` 为占位内容；`host-mount`：宿主
`<workdir>/mount` 是 `fuse` 挂载并以当前 UID 可读；旧版
`POST /api/fs/mount` → `GET /api/fs/mpoint` → `POST /api/fs/unmount`；Antares
`POST /antares/mounts` → `/ready` → 容器内与宿主都列出挂载目录 → `DELETE` → 挂载点目录在
容器内与宿主都已回收（需要含 `remove_mount_dirs` 修复的 ScorpioFS 镜像，`up-scorpio --build`
重建）。服务未启动时输出 `SKIP`（设 `SCORPIOFS_IT=1` 改为失败），单跑一例用
`MEGA2_SMOKE_CASE=<name>`。

联调要点：

- 宿主看到的是同一个 FUSE 挂载；容器内路径 `/mnt/scorpiofs/mount` ⇄ 宿主 `<workdir>/mount`。
  也可进容器看：`docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app --profile scorpio exec -T scorpiofs ls -la /mnt/scorpiofs/mount`。
- `down`/`stop` 走 SIGTERM 优雅卸载（`stop_grace_period: 45s`），宿主挂载随之消失；若容器被
  SIGKILL，宿主会残留 `Transport endpoint is not connected` 的挂载，`up-scorpio` 会拒绝启动并提示
  `sudo umount -l <path>`。跑 `down` 前不要让 shell 停在挂载目录里（EBUSY 会拖慢卸载）。
- 不要只删 `mega2-data` 卷而保留数据库：compose `mega2` 的 blob（local 对象存储）与 vault key 在卷里、
  元数据在 `postgres` 的 `public` schema，拆开删会得到 `core key file is missing` 与 0 字节文件。
  要重置就整栈 `down -v`。
- 排查 API 契约时把日志调到 debug：`MEGA2_IT_SCORPIO_LOG_LEVEL=scorpio=debug ./scripts/dev-test.sh up-scorpio`，
  再 `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app --profile scorpio logs -f scorpiofs`。
- ScorpioFS HTTP API 无认证，端口只绑 `127.0.0.1`；不要改成 `0.0.0.0`。
- Antares CL 层（`/api/v1/cl/{link}/files-list`）只有 review policy 提供；IT 栈 `mega2`
  默认即 review，trunk 栈（`macos-orbstack-mega2-compose.yml` / `linux-mega2-compose.yml` / `docker/docker-compose-storage-only.yml`）不含。

## 聚焦命令

```bash
./scripts/dev-test.sh vault
./scripts/dev-test.sh git-cli
# 等价手贴：
# source .env.test
# cargo test -p mega2 --test integration_vault -- --nocapture --test-threads=1
# cargo test -p mega2 --test integration_git_cli -- --nocapture --test-threads=1
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
| mailpit SMTP | `127.0.0.1:11025` → 1025 | website IT SMTP 捕获（非 mega2） |
| mailpit UI/API | `127.0.0.1:18025` → 8025 | website IT 捕获查看（非 mega2） |
| rustfs S3 API | `127.0.0.1:19000` → 9000 | `MEGA_OBJECT_STORAGE__S3__ENDPOINT_URL` |
| rustfs console | `127.0.0.1:19001` → 9001 | 人工查看 |
| mega2（`app`） | `127.0.0.1:19180` → 8000 | `MEGA2_IT_HTTP_URL` |
| scorpiofs（`scorpio`） | `127.0.0.1:12725` → 2725 | `MEGA2_IT_SCORPIO_URL`（ScorpioFS HTTP API，无认证） |
| git-cli | 无端口映射 | bridge 网络经 `host.docker.internal` 访问宿主高位端口 |

网络名固定为 `mega2-test-network`：带 `-p mega2-it` 与不带 `-p` 的两套栈会争用，启新栈前先 `down -v` 旧栈。

## 环境变量：资源连接 vs harness

**资源连接**（`.env.test` / `.env.test.example`，`export` 后被进程读取）：

- `MEGA_DATABASE__DB_TYPE` / `MEGA_DATABASE__DB_URL`
- `MEGA_REDIS__URL`
- `MAILPIT_API_URL` / `MAILPIT_SMTP_HOST` / `MAILPIT_SMTP_PORT`：仅 website
  认证/产品邮件 IT 捕获；mega2 不读取它们
- `MEGA_NOTIFICATION__WEBSITE_MAIL_BASE_URL` /
  `MEGA_NOTIFICATION__WEBSITE_MAIL_BEARER`：isolated IT 的 website 产品邮件
  API 客户端占位；不是 SMTP 配置
- 可选 S3：`MEGA_OBJECT_STORAGE__STORAGE_TYPE=s3compatible` 及 `MEGA_OBJECT_STORAGE__S3__*`
  （须与 compose 中 `rustfs` / `rustfs_secret` 一致）

**Harness / compose 编排**（`MEGA2_IT_*`，控制 runner 而非业务配置字段名）：

| 变量 | 作用 |
| --- | --- |
| `MEGA2_IT_GIT_WORKDIR` | git-cli 共享宿主根（默认 `/tmp/mega2-git`）；变更后须 `--force-recreate git-cli` |
| `MEGA2_IT_GIT_UID` / `GID` | 容器内用户；本地 `id -u` ≠ 1000 时必设（脚本默认导出当前用户） |
| `MEGA2_IT_HTTP_URL` | 指向 compose `app` 常驻服务 |
| `MEGA2_IT_SCORPIO_URL` | 指向 compose `scorpio` 常驻 ScorpioFS API（默认 `http://127.0.0.1:12725`，供 `scorpiofs_smoke.sh`） |
| `MEGA2_IT_SCORPIO_WORKDIR` | 宿主可见 FUSE 挂载的根（默认 `/tmp/mega2-scorpiofs`；`mount/` 与 `antares/` 以 rshared bind 进容器）；变更后须 `--force-recreate scorpiofs` |
| `MEGA2_IT_SCORPIO_LOG_LEVEL` | `scorpiofs` 容器的 `SCORPIO_LOG_LEVEL`（默认 `info`；联调可设 `scorpio=debug`） |
| `MEGA2_IT_ALLOW_HOST_GIT=1` | **仅**本地实验用宿主机 git；**不是**验收路径 |
| `MEGA2_IT_SKIP_GIT_CLI=1` | 显式跳过 git-cli 用例（非默认门禁） |
| `MEGA2_IT_PROJECT` | Compose 项目名（默认 `mega2-it`；脚本可覆盖） |

不要把 compose **服务名**（如 `postgres`）写进宿主侧 URL——宿主进程一律连 `127.0.0.1:<高位端口>`；容器内（profile `app`）才用服务 DNS 名。

## 故障排查与重置

**栈未就绪 / 连错库**

```bash
docker compose -p mega2-it -f docker/docker-compose.test.yml ps
docker compose -p mega2-it -f docker/docker-compose.test.yml logs postgres redis mailpit rustfs
psql 'postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2' \
  -c "select current_database(), count(*) from seaql_migrations"
```

确认 `source .env.test` 后 URL 指向 `15432` / `16379`，且未误用生产 `config/config.toml` 默认值。

**`git-cli runner unavailable`**

- 是否执行了 `./scripts/dev-test.sh up-full`（或 `--profile git up -d --wait`）？
- 工作根是否已 `mkdir` + `chmod 1777`？UID/GID 是否与宿主一致？
- 宿主机 `git` **不能**替代验收；仅调试时可设 `MEGA2_IT_ALLOW_HOST_GIT=1`。

**git 工作目录 EACCES / 挂载分叉**

```bash
./scripts/dev-test.sh up-full
# 若仍异常，强制重建 git-cli：
dir="${MEGA2_IT_GIT_WORKDIR:-/tmp/mega2-git}"
mkdir -p "$dir" && chmod 1777 "$dir"
export MEGA2_IT_GIT_UID="$(id -u)" MEGA2_IT_GIT_GID="$(id -g)"
docker compose -p mega2-it -f docker/docker-compose.test.yml \
  --profile git up -d --force-recreate --wait git-cli
```

**website 邮件未进 Mailpit**

```bash
curl -fsS http://127.0.0.1:18025/api/v1/messages
```

检查 `website-next` 的测试邮件 provider / SMTP 配置；mega2 没有 SMTP 配置，也不以
Mailpit 可用性作为启动或测试门。注意 IT 栈默认注入的是 `EMAIL_PROVIDER=test`
（进程内内存 provider，**不发 SMTP**），因此 WE-06 通过时 Mailpit 里**本就应当为空**；
要让邮件真正落到 Mailpit，需把 `website-next` 的 `EMAIL_PROVIDER` 改为 `smtp` 并设置
`SMTP_HOST=mailpit` / `SMTP_PORT=1025`。前端仓库为 sibling `../megaui`（`apps/web`）。

**Workspace Code 栈 IT（`WEBSITE_IT=1`）**

`docker/docker-compose.test.yml` 的 `website-next` 服务注入
`MEGA_CODE_DATA_BACKEND=mega2` 与容器内 `MEGA2_PUBLIC_BASE_URL=http://mega2:8000`，
使 megaui `/api/mega` Code 读路径经 BFF 转发 mega2。在 megaui 仓执行：

```bash
WEBSITE_IT=1 pnpm test:api -- tests/api/mega/workspace-code-stack.test.ts
```

需 `--profile app --profile web` 栈已就绪；未设置 `WEBSITE_IT=1` 时该用例自动跳过。

**Workspace 对象存储 IT（FS-02，`website-next`）**

`website-next` 还注入 RustFS S3 环境（保留桶名 **`monoui`**）：

| 变量 | compose 值 |
|------|------------|
| `STORAGE_PROVIDER` | `s3` |
| `S3_BUCKET` | `monoui` |
| `S3_ENDPOINT` | `http://rustfs:9000` |
| `S3_PUBLIC_URL` | `http://127.0.0.1:19000/monoui` |
| `S3_FORCE_PATH_STYLE` | `true` |
| `S3_ACCESS_KEY_ID` / `S3_ACCESS_KEY_SECRET` | `rustfs` / `rustfs_secret` |

`website-next` 依赖 `rustfs-init: service_healthy`（双桶 **`mega2`** + **`monoui`** 已建）。
设计细节见 megaui [`docs/implementation/workspace-storage-backend.md`](../megaui/docs/implementation/workspace-storage-backend.md)。

**干净重置**

```bash
./scripts/dev-test.sh down
# 刷新本地 env（保留自定义注释时请手动合并）
cp .env.test.example .env.test
```

零残留可按 compose project label 检查卷与网络为空（见 `test-infra.md`）。

## 相关文档

- 编排事实源：[`docker/docker-compose.test.yml`](../docker/docker-compose.test.yml)
- Env 模板：[`.env.test.example`](../.env.test.example)
- 测试脚本：[`scripts/dev-test.sh`](../scripts/dev-test.sh)
- 基建规范 / 服务登记：[`docs/refactoring/test-infra.md`](./refactoring/test-infra.md)
- IT 策略与覆盖矩阵：[`docs/refactoring/integration.md`](./refactoring/integration.md)
- Agent 提交门禁：[`AGENTS.md`](../AGENTS.md)
