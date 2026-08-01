# 测试基建规范

本文是 monoengine 测试基建的**单一事实源**：双层测试职责、fixture 生命周期、
docker-compose 测试栈新服务登记 checklist、客户端版本确定性规则。
既有的 rustfs + rustfs-init（S3-compatible）扩展在此按 checklist 登记；后续新服务（含 git-cli
runner）必须先按本节登记并评审，**禁止绕规范直接改 `docker-compose.test.yml`**。

策略与覆盖矩阵仍住在 [`integration.md`](./integration.md)；本文不复制那些内容。

## 双层测试职责

### 模块集成测试（crate 内部）

- **位置：** `src/**/*.rs` 中的 `#[cfg(test)] mod tests`（随 `monoengine-core` 编译）。
- **用途：** 需要直接调用 crate 内部 API 的路径（storage、migration、通知
  触发器等）。可连接 Docker PostgreSQL / Redis；Mailpit 仅属于 website 邮件
  IT，monoengine 用例不得把它当作 SMTP 投递依赖。
- **边界：** 允许 `use crate::...`；不拉起真实 `monoengine` 二进制。

### 黑盒进程测试（cargo-native）

- **位置：** `bin/tests/integration_*.rs`（属于 `monoengine` 二进制 crate）。
- **用途：** CLI、启动顺序、HTTP / Git 协议 smoke、secret 不回显、进程退出码。
- **边界：** 通过 `CARGO_BIN_EXE_monoengine` 与外部协议断言；**不**导入
  `crate::...`。共享编排 helper 放在 `bin/tests/common/mod.rs`。
  另：compose `monoengine`（profile `app`）提供栈级常驻 HTTP，供
  `integration_compose_monoengine_http_smoke` / CI 探针使用；**不**替代本层
  的 per-case 进程隔离。

单元测试（纯逻辑、无外部依赖）仍放在对应源文件的 `#[cfg(test)]` 中，不另立规范。

## fixture 生命周期

1. **每用例隔离：** 独立临时目录（`MEGA_BASE_DIR` / `MEGA_CONFIG` / cache），
   不得共享可变全局状态。
2. **数据面：** 使用 `docker-compose.test.yml` 暴露的测试栈端口；变量经
   `source .env.test` 注入（示例见仓库根 `.env.test.example`，全部为 `export`）。
3. **进程：** 黑盒用例启动的 `service http` 必须在 teardown 中停止；不得泄漏
   监听端口或子进程。
4. **compose：** 规范项目名为 `monoengine-it`。凡按本文启停的命令必须显式带
   `-p monoengine-it`（`up` / `run` / `down -v` / 残留查询同一项目名），并以
   compose project label 判定零残留卷与网络。启动时用了哪个 `-p`，清理时必须用
   同一个；禁止把 `-p monoengine-it` 的 `down` 拿去清理未带 `-p` 的默认项目（目录名
   `monoengine`），也禁止反过来。`networks.default` 在 compose 文件里固定名为
   `monoengine-test-net`，因此**带 `-p` 与不带 `-p` 的两套栈不可并存**——会争用同一
   网络名；启新栈前须先停掉另一套（`down -v`）。仓库内尚存的、未带 `-p` 的历史
   命令在对应任务卡改写前仍指向默认项目。
5. **共享宿主路径：** git-cli 等服务挂载固定共享目录
   `${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-git}`。启栈前应：
   `mkdir -p "$dir" && chmod 1777 "$dir"`（保证测试 UID 可写）。若目录缺失，
   Docker bind 可能以 `root:root` 自动创建，随后未提权的 cargo 进程分配子目录
   会 `EACCES`——因此本地与 CI 启动脚本都必须先 mkdir（`config-validation.yml`
   的 `validate-config` 已在 `--profile git up` 前执行）。用例只在该根下分配子目录并清理。变更
   `MONOENGINE_IT_GIT_WORKDIR` 后必须 `--force-recreate git-cli`（或整栈），
   否则 host 与容器挂载会静默分叉。`git-cli` 在 `profiles: ["git"]` 下，默认
   `up -d --wait` **不会**启动它（host 网络仅在 Linux 验收路径启用；`--profile git`
   也避免在非目标开发机上误把 host 网络拉进默认栈）。**集成测试 / git-cli harness
   的目标 OS 是 Linux**：跑全量门（`source .env.test && cargo test --all`）必须
   额外启动 git-cli——在基础栈已 `up -d --wait` 后执行
   `docker compose -p monoengine-it -f docker-compose.test.yml --profile git up -d --wait git-cli`
   （或等价的带 `--profile git` 的整栈 `up`），否则 `integration_git_cli`
   会以 `git-cli runner unavailable` 硬失败。宿主机 `git` **不是**跨平台降级路径；
   仅 Linux 本地实验可显式设 `MONOENGINE_IT_ALLOW_HOST_GIT=1`（见下方登记条目）。
   默认以 UID/GID `1000:1000` 运行；本地测试 UID 不同时先
   `export MONOENGINE_IT_GIT_UID=$(id -u) MONOENGINE_IT_GIT_GID=$(id -g)` 再
   `up`/`--force-recreate`。

## 新服务登记 checklist

新增任何测试栈服务前，必须按下列清单完成书面登记（写入本文「已登记服务」），
经评审后再改 `docker-compose.test.yml`：

1. **镜像固定 tag 或 digest**（禁止 `latest`）。
2. **端口绑定 `127.0.0.1`**（不暴露到 `0.0.0.0`）。
3. **使用高位端口**（避免与开发机常见服务冲突）。
4. **healthcheck 定义**（`up -d --wait` 可等待）。
5. **`down -v` 清理**（文档与 CI 均要求；失败路径也要清理）。
6. **零残留判据**（按 compose project label 检查卷与网络为空）。
7. **secret 只经 stdin/env 注入**（不把凭据写进镜像层或 compose 明文文件之外的
   可提交产物；测试口令仅限公开示例）。
8. **`::add-mask::` 脱敏**（CI 日志对凭据打码）。
9. **目标 OS = Linux**（集成测试与 `git-cli` harness 不要求 Windows/macOS 兼容路径；
   宿主机客户端仅作显式本地实验 opt-in，不得冒充固定版本验收）。

另需在登记条目中写明：网络模式（与 `networks.default` 的互斥约束）、卷/工作目录约定、
`profiles` / `depends_on` 语义、CI 入口（若有）。

## 客户端版本确定性规则

- 通过 compose 提供的客户端（如 git-cli runner）必须在登记条目中写明**固定版本字符串**，
  并用 Verification 断言容器内 `git --version`（及同类工具）等于该 pin。
- CI 宿主机客户端（`git-protocol-smoke.yml`）固定版本（与 job `env` / summary 断言同源）：
  - CI git 固定版本：`2.54.0`
  - CI git-lfs 固定版本：`3.7.1`
  实际 `git --version` / `git lfs version` 解析出的版本号必须与上述 pin 完全相等，否则 job 失败；
  两个版本输出写入 `$GITHUB_STEP_SUMMARY`。
- 升级镜像 tag / digest 或 pin 值必须是显式 PR 动作，并同步更新登记条目。

## 已登记服务

### mailpit（website 邮件捕获）

| 项 | 值 |
|---|---|
| 服务名 | `mailpit` |
| 用途 | 可选的 website-next 认证和产品邮件 SMTP 捕获；**不是 monoengine 服务依赖或测试门** |
| 端口 | `127.0.0.1:11025:1025`（SMTP）、`127.0.0.1:18025:8025`（UI/API） |
| profiles | 无；默认数据面可启动，但仅在 website 邮件 IT 需要时消费 |
| depends_on | monoengine 不依赖它。website-next 若选择 SMTP 测试 provider，可连接 `mailpit:1025` |
| CI 入口 | 仅 website 邮件 IT 需要捕获时使用；`config-validation.yml` 不要求 monoengine SMTP 成功路径 |
| 清理 | `docker compose -p monoengine-it -f docker-compose.test.yml down -v` |

`.env.test.example` 的 `MAILPIT_*` 变量同样仅供 website IT；monoengine 不读取它们。

### rustfs + rustfs-init（S3-compatible 对象存储）

| 项 | 值 |
|---|---|
| 服务名 | `rustfs`、`rustfs-init` |
| 镜像 | `rustfs/rustfs:1.0.0-beta.11@sha256:84ce557a0245a06a9aae5516f55ee0f007fca78d41df356f419306fdc0cb168c`；桶初始化客户端 `minio/mc:RELEASE.2025-04-16T18-13-26Z`（固定 tag，非 `latest`；`mc` 仅作通用 S3 客户端） |
| 端口 | `127.0.0.1:19000:9000`、`127.0.0.1:19001:9001`（高位 + 仅回环；S3 API / console） |
| healthcheck | `rustfs`：`curl -f http://127.0.0.1:9000/health`（见 `docker-compose.test.yml`） |
| 网络 | 默认 `networks.default` → `monoengine-test-net` |
| 卷 / 工作目录 | `rustfs` 使用容器内路径 `/data`（`RUSTFS_VOLUMES=/data`），**无**宿主机 bind-mount；单盘本地 smoke 设 `RUSTFS_UNSAFE_BYPASS_DISK_CHECK=true`。数据仅存在于该容器可写层，`down -v` 后不保留。`rustfs-init` 无持久卷（one-shot） |
| profiles | `rustfs-init` 使用 `profiles: ["init"]`：**one-shot** 桶初始化，不参与默认 `up -d --wait`（`--wait` 要求服务保持 running/healthy）；显式执行 `docker compose -p monoengine-it -f docker-compose.test.yml --profile init run --rm rustfs-init` |
| depends_on | `rustfs-init` → `rustfs` 且 `condition: service_healthy` |
| 清理 | `docker compose -p monoengine-it -f docker-compose.test.yml down -v`；零残留按 project label 判定 |
| CI 入口 | `.github/workflows/config-validation.yml` 的 `validate-config` job：先 `mkdir -p` + `chmod 1777` 共享 git 工作根并导出 `MONOENGINE_IT_GIT_UID/GID=$(id -u/g)`，再 `docker compose -p monoengine-it -f docker-compose.test.yml --profile git up -d --wait` 拉起含 rustfs 与 git-cli 的栈，随后 `--profile init run --rm rustfs-init` 建桶；执行面含 `cargo test -p monoengine --test integration_vault`、`--test integration_website_auth`、`--test integration_git_cli`（产品邮件投递由 website 负责，本 job **不**跑本仓 SmtpMailer→Mailpit 或 `integration_mail_dispatcher_*`）；job 末尾 `if: always()` 下 `-p monoengine-it --profile git --profile app --profile web down -v` |
| secret | 公开测试凭据 `rustfs` / `rustfs_secret`（仅测试栈，与 `RUSTFS_ACCESS_KEY`/`RUSTFS_SECRET_KEY` 及 `.env.test.example` 对齐）；CI 对同类凭据使用 `::add-mask::` |
| 降级 | 无客户端版本 pin 需求；本地可不启 rustfs（相关 gate 自行 skip/opt-in） |

对照锚点：`docker-compose.test.yml` 的 `rustfs` / `rustfs-init` 服务块与 `networks.default`。

### git-cli（首个按 checklist 落地的扩展服务）

git-cli 固定版本: `git version 2.49.1`

| 项 | 值 |
|---|---|
| 服务名 | `git-cli` |
| 镜像 | `alpine/git:v2.49.1@sha256:c0280cf9572316299b08544065d3bf35db65043d5e3963982ec50647d2746e26`（固定 tag + digest，非 `latest`） |
| 固定版本字符串 | 见上文 `git-cli 固定版本`（容器内 `git --version` 必须与该字符串完全相等） |
| 端口 | 无独立端口映射；通过 `network_mode: host` 访问宿主 `127.0.0.1` 高位端口 |
| healthcheck | `CMD git --version`（见 `docker-compose.test.yml`） |
| 网络 | **`network_mode: host`**，因此**不**加入 `networks.default`（与 host 网络互斥，见 ADR-IT-01） |
| 卷 / 工作目录 | 挂载共享宿主路径 `${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-git}` → 容器 `/work`（`working_dir: /work`）。**启栈前应由宿主机预创建且对测试 UID 可写**（推荐 `mkdir -p "$dir" && chmod 1777 "$dir"`）。若缺失，Docker 可能以 `root:root` 自动建目录，导致后续未提权进程 `EACCES`；CI `validate-config` 已在 `--profile git up` 前 mkdir（见「CI 入口」）。该路径跨用例可见；用例只在其下自建**子目录**并清理。相对路径按 compose 文件所在目录（仓库根）解析，与 harness `git_cli_workdir()` 对齐（不以 `bin/` CWD 为准）。**`MONOENGINE_IT_GIT_WORKDIR` 仅在容器创建时解析**：改根路径必须用同一环境变量值执行 `docker compose -p monoengine-it -f docker-compose.test.yml up -d --force-recreate git-cli`（或整栈 recreate）；已在跑的栈上事后 `export` 新值不会改挂载 |
| 运行身份 | `user: "${MONOENGINE_IT_GIT_UID:-1000}:${MONOENGINE_IT_GIT_GID:-1000}"`（Compose 可解析的数值默认，不依赖 Bash 未 export 的 `$UID`）。默认 `1000:1000` 对齐常见 CI runner；本地若 `id -u` 不是 1000，启栈前必须 `export MONOENGINE_IT_GIT_UID=$(id -u) MONOENGINE_IT_GIT_GID=$(id -g)`。变更 UID/GID 后需要 `--force-recreate git-cli` |
| profiles | `profiles: ["git"]`（**不**参与默认 `up -d --wait`；验收路径在 Linux 上显式 `--profile git`。profile 也避免非目标开发机误启 host 网络拖垮数据面） |
| depends_on | 无 |
| 清理 | `docker compose -p monoengine-it -f docker-compose.test.yml --profile git down -v`（或整栈 `down -v`）；零残留按 project label 判定 |
| CI 入口 | `.github/workflows/config-validation.yml` 的 `validate-config`：mkdir 工作根、导出 `MONOENGINE_IT_GIT_UID/GID=$(id -u/g)` 后 `--profile git up -d --wait`，再跑 `cargo test -p monoengine --test integration_git_cli -- --test-threads=1`；`.github/workflows/git-protocol-smoke.yml` 在协议路径变更时同样先拉起 `git-cli`，再以 `cargo test -p monoengine --release --test integration_git_cli` 跑同一 target（复用本 job 的 release 构建；补 allowlist A 未含协议路径的覆盖缺口），其后用宿主机 git（pin 见「客户端版本确定性规则」）跑脚本矩阵；该 job `timeout-minutes: 60`（release 构建 + cargo gate + shell smoke） |
| secret | **不**注入任何 secret；凭据由用例经 credential helper / env 注入 |
| 目标 OS / 降级 | **Linux only**。compose `git-cli`（pin `git version 2.49.1`）是唯一验收 runner。宿主机 `git` 仅当显式 `MONOENGINE_IT_ALLOW_HOST_GIT=1` 时用于 Linux 本地实验，且不得冒充固定版本门；无 Windows/macOS 兼容路径 |

### monoengine（compose 常驻 HTTP，profile `app`）

被测产品进程进入 compose 拓扑的登记条目。**与 cargo 黑盒分层并存**：`bin/tests/integration_*.rs` 仍通过 `CARGO_BIN_EXE_monoengine` 按用例拉起隔离进程（唯一端口 / 临时 `MEGA_BASE_DIR` / 隔离 DB）；本服务提供**栈级常驻** `service http`，供 compose smoke、手工联调与 CI 健康探针使用，**不**替代 per-case 隔离门。

| 项 | 值 |
|---|---|
| 服务名 | `monoengine` |
| 镜像 | `monoengine:local`（`pull_policy: never`）。本地源码构建：`Dockerfile`（context = 含 `monoengine/`+`orbit/` 的父目录，bookworm）。CI/快速路径：宿主机 `cargo build -p monoengine` 后用 `Dockerfile.it-runtime`（**Ubuntu 24.04**，匹配较新 glibc）打包 `monoengine.itbin` |
| 固定版本字符串 | 无客户端 pin；镜像内容随源码 / 宿主机二进制变化，升级为显式 build |
| 端口 | `127.0.0.1:19180:8000`（容器内 CLI 默认 `8000`；仅回环） |
| healthcheck | `curl -f http://127.0.0.1:8000/api/openapi.json` |
| 网络 | 默认 `networks.default` → `monoengine-test-net`（可解析 `postgres`/`redis` 服务名） |
| 卷 / 工作目录 | named volume `monoengine-data` → `/var/lib/monoengine`（`MEGA_BASE_DIR`）；对象存储默认 local → `/var/lib/monoengine/objects` |
| profiles | `profiles: ["app"]`：**不**参与默认 `up -d --wait`；显式 `--profile app` |
| depends_on | `postgres`、`redis`（`service_healthy`）。`MEGA_OAUTH__WEBSITE_API_BASE_URL=http://website-next:7001`；`MEGA_OAUTH__ALLOWED_CORS_ORIGINS` 保留既有 IT origin 并含 `http://127.0.0.1:17001`。不声明对 `website-next` 的 `depends_on`：该服务仅属 `web` profile，而 `monoengine` 属 `app`；跨 profile 依赖会使 app-only smoke 无法解析。会话联调使用下方规定的 web-first 启动顺序；不依赖 Mailpit 或 RustFS。 |
| 清理 | `docker compose -p monoengine-it -f docker-compose.test.yml --profile app down -v`（或与 `--profile git` 一并） |
| CI 入口 | `.github/workflows/config-validation.yml`：先检查并 checkout private `genedna/website` sibling（缺 `WEBSITE_CHECKOUT_TOKEN`，可回退 `ORBIT_CHECKOUT_TOKEN`，与 orbit 纪律一致），在数据面 `up` 后用 `Dockerfile.it-runtime` 打 `monoengine:local`，再以 `--profile app --profile web up -d --wait` 同启服务，探测 `19180/api/openapi.json` 与 `17001/api/auth/get-session`；job 运行 `WEBSITE_IT=1 cargo test -p monoengine --test integration_website_auth -- --test-threads=1`，并在 `if: always()` 带两个 profile `down -v`。 |
| secret | 无注入生产 secret；DB 使用公开测试口令 `monoengine_test_password`（用户/库名均为 `monoengine`）；website-mail bearer 仅为公开的隔离 IT 值 |
| 降级 / 黑盒 | 栈级 smoke：`integration_compose_monoengine_http_smoke`（端口未监听时 soft-skip）。隔离黑盒仍用 `CARGO_BIN_EXE` |

本地源码构建示例：

```bash
# 需 sibling ../orbit
docker compose -p monoengine-it -f docker-compose.test.yml --profile app build monoengine
docker compose -p monoengine-it -f docker-compose.test.yml up -d --wait postgres redis
docker compose -p monoengine-it -f docker-compose.test.yml --profile app up -d --wait monoengine
curl -sf http://127.0.0.1:19180/api/openapi.json >/dev/null
```

CI / 宿主机二进制打包示例：

```bash
cargo build -p monoengine
cp target/debug/monoengine monoengine.itbin
docker build -f Dockerfile.it-runtime -t monoengine:local .
rm -f monoengine.itbin
docker compose -p monoengine-it -f docker-compose.test.yml --profile app up -d --wait monoengine
```

### website-next + website-db-init（Better Auth IT，profile `web`）

| 项 | 值 |
|---|---|
| 服务名 | `website-db-init`、`website-next` |
| 镜像 / 构建 | `website-db-init` 从 sibling `../website` 的 `apps/next-app/Dockerfile` `builder` stage 构建，复用已安装的 `pnpm` / Drizzle 工具；该 Dockerfile 也复制根目录 `drizzle.config.sqlite.ts`，供 init target 解析 schema。`website-next:local`，`pull_policy: never`，从同一 Dockerfile 的 runner stage 构建。本地和 CI 都必须有该 checkout；CI 应缓存构建层但不得尝试拉取同名远程镜像 |
| 端口 | `127.0.0.1:17001:7001`（仅回环；next-app Dockerfile 的 `EXPOSE 7001`） |
| healthcheck | 容器内 Node TCP 连接 `127.0.0.1:7001`；只在 Next 进程已监听时 healthy |
| 网络 | 默认 `networks.default` → `monoengine-test-net`，可由后续 `monoengine` profile 通过 `website-next:7001` 访问 |
| 账户库 / 卷 | 两服务都设 `DB_DIALECT=sqlite` 与 `SQLITE_DB_PATH=/app/data/website-it.sqlite`，共享 named volume `website-next-data` → `/app/data`。website 的 Next config 保持该路径为运行时环境变量（不在 build 时内联默认 `local.sqlite`）。`website-db-init` 运行 `pnpm db:push:sqlite`，再将目录交给 runner 的 `nextjs` UID/GID `1001:1001`；因此每个新卷在 Next 启动前已有 schema，且运行时仍可写入账户与 session。该数据库不设置 `DATABASE_URL`，不连接或复用 `monoengine` PostgreSQL |
| profiles | 两服务均为 `profiles: ["web"]`，不参与默认 `up -d --wait`；显式启动：`docker compose -p monoengine-it -f docker-compose.test.yml --profile web up -d --wait website-next` |
| depends_on / 会话联调顺序 | `website-next` → `website-db-init`，`condition: service_completed_successfully`；初始化失败时 Next 不会启动。没有 `monoengine` 跨 profile 依赖：`website-next` 是 `web` profile、`monoengine` 是 `app` profile，避免 app-only smoke 被 web 拉起。可联合运行 `docker compose -p monoengine-it -f docker-compose.test.yml --profile app --profile web up -d --wait`，此时 Compose 先完成 schema 初始化，再等待两个常驻服务 healthy |
| 清理 | `docker compose -p monoengine-it -f docker-compose.test.yml --profile web down -v`；删除 `website-next-data` 后 SQLite 账户状态不保留，零残留仍按 compose project label 判定 |
| CI 入口 | `.github/workflows/config-validation.yml` 强制 checkout sibling `../website`（`genedna/website` @ `9afef8f`；缺 `WEBSITE_CHECKOUT_TOKEN` 时可回退 `ORBIT_CHECKOUT_TOKEN`）。构建 `monoengine:local` 后以 `--profile app --profile web up -d --wait` 同启，设置 `WEBSITE_IT=1` 跑 `cargo test -p monoengine --test integration_website_auth -- --test-threads=1`，并在 `if: always()` 使用相同 profile `down -v`。Dockerfile 内容守卫保留为回归保护。 |
| secret | `BETTER_AUTH_SECRET` 是仅用于本地 IT 的公开固定值；不得替换为或记录生产 secret |
| 性能 | 首次 source build 预算 ≤ 20 分钟；默认 profile 不构建、不启动该服务 |

对照锚点：`docker-compose.test.yml` 的 `website-db-init` / `website-next` 服务块；拓扑语义见
[`website-auth.md`](./website-auth.md) §5。

会话服务连通性 smoke（未登录响应可为 `null`，但不得是连接错误）：

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app --profile web exec -T monoengine \
  curl -fsS http://website-next:7001/api/auth/get-session
```

栈级会话黑盒（ITW-03）：

```bash
source .env.test
WEBSITE_IT=1 cargo test -p monoengine --test integration_website_auth -- --test-threads=1 --nocapture
```

未设置 `WEBSITE_IT=1` 时该 target 会明确输出 `SKIP`，供默认数据面循环使用；设置后，
`website-next:17001` 或 `monoengine:19180` 不可达即测试失败，因此 CI 不会将跳过误记为通过。

## 强制纪律

**新增服务必须先按 checklist 登记评审，禁止绕规范加服务。**
