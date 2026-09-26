# 测试基建规范

本文是 mega2 测试基建的**单一事实源**：双层测试职责、fixture 生命周期、
docker-compose 测试栈新服务登记 checklist、客户端版本确定性规则。
既有的 rustfs + rustfs-init（S3-compatible）扩展在此按 checklist 登记；后续新服务（含 git-cli
runner）必须先按本节登记并评审，**禁止绕规范直接改 `docker/docker-compose.test.yml`**。

策略与覆盖矩阵仍住在 [`integration.md`](./integration.md)；本文不复制那些内容。

## 双层测试职责

### 模块集成测试（crate 内部）

- **位置：** `src/**/*.rs` 中的 `#[cfg(test)] mod tests`（随 `mega2-core` 编译）。
- **用途：** 需要直接调用 crate 内部 API 的路径（storage、migration、通知
  触发器等）。可连接 Docker PostgreSQL / Redis。本仓 compose 数据面不启动
  SMTP 捕获服务；mega2 用例不得把它当作投递依赖。
- **边界：** 允许 `use crate::...`；不拉起真实 `mega2` 二进制。

### 黑盒进程测试（cargo-native）

- **位置：** `bin/tests/integration_*.rs`（属于 `mega2` 二进制 crate）。
- **用途：** CLI、启动顺序、HTTP / Git 协议 smoke、secret 不回显、进程退出码。
- **边界：** 通过 `CARGO_BIN_EXE_mega2` 与外部协议断言；**不**导入
  `crate::...`。共享编排 helper 放在 `bin/tests/common/mod.rs`。
  另：compose `mega2`（profile `app`）提供栈级常驻 HTTP，供
  `integration_compose_mega2_http_smoke` / CI 探针使用；**不**替代本层
  的 per-case 进程隔离。

### 栈级 Compose 黑盒（storage-only / trunk）

- **位置：** `scripts/git_protocol_smoke_storage_only.sh`，由
  `docker/docker-compose-storage-only.yml` 的 `--profile smoke` / `git-smoke` 驱动
  （项目名 `mega2-trunk`）。运维入口见 [`deploy-trunk.md`](../deploy-trunk.md) §8.1。
- **用途：** 对常驻 storage-only 栈的 Git 协议面黑盒（真实 **`git` / `git-lfs` / `ssh`**
  客户端经已发布端口）。结果经宿主 stdout / 可选 `tee` 到 `target/tmp`。
- **边界：** **不**使用 `libra` 作协议客户端（本仓 VCS 与冒烟观察者正交）；
  **不**替代 `CARGO_BIN_EXE` 黑盒 IT；review/CL 冒烟仍用
  `scripts/git_protocol_smoke.sh`，勿在 trunk 上当作成功门。
- **API 写 → Git 可见性（plan-20260904）：** `scripts/api_write_smoke_storage_only.sh`
  （`curl` + `git`）可与协议 smoke 并列在同一 `git-smoke` 容器执行；运维入口见
  [`deploy-trunk.md`](../deploy-trunk.md)。该脚本独立于 plan-20260906 的 SO case
  编号；**plan-20260906** 后续可吸收 `api_write_smoke_storage_only.sh` 的稳定
  case 名为附加 VER（本仓不要求 60906 已吸收才算 AW-04 完成）。

单元测试（纯逻辑、无外部依赖）仍放在对应源文件的 `#[cfg(test)]` 中，不另立规范。

## fixture 生命周期

1. **每用例隔离：** 独立临时目录（`MEGA_BASE_DIR` / `MEGA_CONFIG` / cache），
   不得共享可变全局状态。
2. **数据面：** 使用 `docker/docker-compose.test.yml` 暴露的测试栈端口；变量经
   `source .env.test` 注入（示例见仓库根 `.env.test.example`，全部为 `export`）。
3. **进程：** 黑盒用例启动的 `service http` 必须在 teardown 中停止；不得泄漏
   监听端口或子进程。
4. **compose：** 规范项目名为 `mega2-it`。凡按本文启停的命令必须显式带
   `-p mega2-it`（`up` / `run` / `down -v` / 残留查询同一项目名），并以
   compose project label 判定零残留卷与网络。启动时用了哪个 `-p`，清理时必须用
   同一个；禁止把 `-p mega2-it` 的 `down` 拿去清理未带 `-p` 的默认项目（目录名
   `mega2`），也禁止反过来。`networks.default` 固定名为
   `mega2-test-network`，因此**带 `-p` 与不带 `-p` 的两套栈不可并存**——会争用同一
   网络名；启新栈前须先停掉另一套（`down -v`）。仓库内尚存的、未带 `-p` 的历史
   命令在对应任务卡改写前仍指向默认项目。
5. **共享宿主路径：** git-cli 等服务挂载固定共享目录
   `${MEGA2_IT_GIT_WORKDIR:-/tmp/mega2-git}`。启栈前应：
   `mkdir -p "$dir" && chmod 1777 "$dir"`（保证测试 UID 可写）。若目录缺失，
   Docker bind 可能以 `root:root` 自动创建，随后未提权的 cargo 进程分配子目录
   会 `EACCES`——因此本地启动脚本必须先 mkdir（`./scripts/dev-test.sh up-full` 会先执行；历史上的 CI
   `config-validation.yml` 的 `validate-config` 也在 `--profile git up` 前执行）。用例只在该根下分配子目录并清理。变更
   `MEGA2_IT_GIT_WORKDIR` 后必须 `--force-recreate git-cli`（或整栈），
   否则 host 与容器挂载会静默分叉。`git-cli` 在 `profiles: ["git"]` 下，默认
   `up -d --wait` **不会**启动它（避免数据面-only 开发循环误拉 runner；
   `--profile git` 显式启用）。**集成测试 / git-cli harness** 在 **Linux 与
   macOS Docker Desktop** 上验收：cargo 在宿主绑定 `127.0.0.1` 高位端口，compose
   `git-cli` 经 bridge + `host.docker.internal` 访问。跑全量门
   （`source .env.test && cargo test --all`）必须额外启动 git-cli——在基础栈已
   `up -d --wait` 后执行
   `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile git up -d --wait git-cli`
   （或等价的带 `--profile git` 的整栈 `up`），否则 `integration_git_cli`
   会以 `git-cli runner unavailable` 硬失败。宿主机 `git` **不是**跨平台降级路径；
   仅本地实验可显式设 `MEGA2_IT_ALLOW_HOST_GIT=1`（见下方登记条目）。
   默认以 UID/GID `1000:1000` 运行；本地测试 UID 不同时先
   `export MEGA2_IT_GIT_UID=$(id -u) MEGA2_IT_GIT_GID=$(id -g)` 再
   `up`/`--force-recreate`。

## 新服务登记 checklist

新增任何测试栈服务前，必须按下列清单完成书面登记（写入本文「已登记服务」），
经评审后再改 `docker/docker-compose.test.yml`：

1. **镜像固定 tag 或 digest**（禁止 `latest`）。
2. **端口绑定 `127.0.0.1`**（不暴露到 `0.0.0.0`）。
3. **使用高位端口**（避免与开发机常见服务冲突）。
4. **healthcheck 定义**（`up -d --wait` 可等待）。
5. **`down -v` 清理**（文档与 CI 均要求；失败路径也要清理）。
6. **零残留判据**（按 compose project label 检查卷与网络为空）。
7. **secret 只经 stdin/env 注入**（不把凭据写进镜像层或 compose 明文文件之外的
   可提交产物；测试口令仅限公开示例）。
8. **`::add-mask::` 脱敏**（CI 日志对凭据打码）。
9. **目标 OS = Linux + macOS Docker Desktop**（集成测试与 `git-cli` harness 通过
   bridge + `host.docker.internal` 跨平台；Windows 未验收）。
   宿主机客户端仅作显式本地实验 opt-in，不得冒充固定版本验收。

另需在登记条目中写明：网络模式（与 `networks.default` 的互斥约束）、卷/工作目录约定、
`profiles` / `depends_on` 语义、CI 入口（若有）。

## 客户端版本确定性规则

- 通过 compose 提供的客户端（如 git-cli runner）必须在登记条目中写明**固定版本字符串**，
  并用 Verification 断言容器内 `git --version`（及同类工具）等于该 pin。
- CI 宿主机客户端（`git-protocol-smoke.yml`；**历史登记**：该 workflow 已不在 `.github/workflows/`（仅剩 `docker.yml`），本文其它「CI 入口」中的 `config-validation.yml` 同样只是历史登记）固定版本（与 job `env` / summary 断言同源）：
  - CI git 固定版本：`2.53.0`（2026-08-31 起；随 self-hosted runner 迁移由 `2.55.0` 改为跟随 runner 机器实际安装的 git 版本，不再随 GitHub `ubuntu-latest` 镜像滚动。宿主机 git 由 runner 机器管理员维护、workflow 不 apt 升级，因此升级 runner 机器 git 时必须同步显式上调这个 pin——`git-protocol-smoke` 在 `Install and pin git` 步骤直接失败，正是为了逼出这次显式决定）
  - CI git-lfs 固定版本：`3.7.1`（runner 机器未预装；由 `Install and pin git / git-lfs` 步骤按此 pin 下载固定 GitHub release 制品自动安装）
  实际 `git --version` / `git lfs version` 解析出的版本号必须与上述 pin 完全相等，否则 job 失败；
  两个版本输出写入 `$GITHUB_STEP_SUMMARY`。
- 本机宿主 git-lfs（现状，2026-09-26）：`tests/integration_git_lfs.rs` 的 `assert_host_git_lfs_pinned`（PATH 优先 `~/.local/bin`）断言宿主 git-lfs 为 `git-lfs/3.8.0`（只断言 git-lfs，不断言宿主 git 的版本）。这是上文「宿主 git 只作本地实验、不作固定版本门」的例外：`integration_git_lfs` 的 trunk LFS 与 storage-events 用例直接调用宿主 git / git-lfs。
- 升级镜像 tag / digest 或 pin 值必须是显式 PR 动作，并同步更新登记条目。

## 已登记服务

本仓默认数据面是 postgres / redis / rustfs / rustfs-init。SMTP 捕获容器
**不是**本仓必起服务，也不在 `docker/docker-compose.test.yml` 登记。

### rustfs + rustfs-init（S3-compatible 对象存储）

| 项 | 值 |
|---|---|
| 服务名 | `rustfs`、`rustfs-init` |
| 镜像 | `rustfs/rustfs:1.0.0@sha256:8cc9801755448b71a786705ce76692c77e14936cccd87cf2fc31842e58f4d1ff`；桶初始化客户端 `rustfs/rc:v0.1.36@sha256:ab024bfebee49a750ce886b4c70963ccd9ddaa03f491704a90710641d7a26699`（固定 tag，非 `latest`；`rc` 为 RustFS 官方 S3 客户端，不使用 MinIO `mc`） |
| 端口 | `127.0.0.1:19000:9000`、`127.0.0.1:19001:9001`（高位 + 仅回环；S3 API / console） |
| healthcheck | `rustfs`：`curl -f http://127.0.0.1:9000/health`；`rustfs-init`：`rc ls local/mega2` **且** `rc ls local/monoui`（建桶后 `sleep infinity`，见 `docker/docker-compose.test.yml`） |
| 网络 | 默认 `networks.default` → `mega2-test-network` |
| 卷 / 工作目录 | `rustfs` 使用容器内路径 `/data`（`RUSTFS_VOLUMES=/data`），**无**宿主机 bind-mount；单盘本地 smoke 设 `RUSTFS_UNSAFE_BYPASS_DISK_CHECK=true`。数据仅存在于该容器可写层，`down -v` 后不保留。`rustfs-init` 无持久卷（建桶后常驻，供 `--wait`） |
| profiles | 无；两者均参与默认 `up -d --wait`。`rustfs-init` 在 `rustfs` healthy 后幂等创建 **`mega2`** 与 **`monoui`** 桶，再以 healthcheck 报告就绪（纯 one-shot exit 会让 `--wait` 失败） |
| depends_on | `rustfs-init` → `rustfs` 且 `condition: service_healthy` |
| 清理 | `docker compose -p mega2-it -f docker/docker-compose.test.yml down -v`；零残留按 project label 判定 |
| CI 入口 | （历史登记，该 workflow 已不在仓库中）`.github/workflows/config-validation.yml` 的 `validate-config` job：先 `mkdir -p` + `chmod 1777` 共享 git 工作根并导出 `MEGA2_IT_GIT_UID/GID=$(id -u/g)`，再 `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile git up -d --wait` 拉起含 rustfs、`rustfs-init`（建桶）与 git-cli 的栈；执行面含 `cargo test -p mega2 --test integration_vault`、`--test integration_website_auth`、`--test integration_git_cli`（本 job **不**跑本仓 SMTP 投递门）；job 末尾 `if: always()` 下 `-p mega2-it --profile git --profile app --profile web down -v` |
| secret | 公开测试凭据 `rustfs` / `rustfs_secret`（仅测试栈，与 `RUSTFS_ACCESS_KEY`/`RUSTFS_SECRET_KEY` 及 `.env.test.example` 对齐）；CI 对同类凭据使用 `::add-mask::` |
| 降级 | 无客户端版本 pin 需求；本地可不启 rustfs（相关 gate 自行 skip/opt-in） |

对照锚点：`docker/docker-compose.test.yml` 的 `rustfs` / `rustfs-init` 服务块与 `networks.default`。

### git-cli（首个按 checklist 落地的扩展服务）

git-cli 固定版本: `git version 2.49.1`

git-cli runner git-lfs 固定版本: `git-lfs/3.8.0`（GM-05 起随镜像内置；healthcheck 断言前缀；2026-09-26 由 `3.7.1` 升级，与 `assert_host_git_lfs_pinned` 断言的宿主 pin 同步）

| 项 | 值 |
|---|---|
| 服务名 | `git-cli` |
| 镜像 | `mega2-git-cli:3.8.0`（本地构建，`build: Dockerfile.git-cli`，GM-05 起）。基底为原登记的固定 digest `alpine/git:v2.49.1@sha256:c0280cf9572316299b08544065d3bf35db65043d5e3963982ec50647d2746e26`，叠加 sha256 校验安装的 git-lfs `3.8.0` 与 `gitcli`（uid 1000）用户；无 `latest`，git pin 不变 |
| 固定版本字符串 | 见上文 `git-cli 固定版本`（容器内 `git --version` 必须与该字符串完全相等）；`git lfs version` 输出必须以上文 git-lfs pin 前缀开头 |
| 端口 | 无独立端口映射；经 `host.docker.internal` 访问宿主 `127.0.0.1` 高位端口 |
| healthcheck | `CMD-SHELL git --version >/dev/null && git lfs version \| grep -q '^git-lfs/3[.]8[.]0 '`（见 `docker/docker-compose.test.yml`） |
| entrypoint / init | `entrypoint: ["sleep","infinity"]`（常驻供 `exec`）；`init: true`（回收 exec 超时包装器遗留的 git/ssh 子进程，GM-08 起） |
| 网络 | **加入 `networks.default`**；`extra_hosts: host.docker.internal:host-gateway`。Harness 将 git remote URL 映射为 `http://host.docker.internal:<port>/`（容器 runner）或 `127.0.0.1`（宿主机 opt-in runner）；宿主侧 curl/TcpStream 仍用 loopback（ADR-IT-01 修订） |
| 卷 / 工作目录 | 挂载共享宿主路径 `${MEGA2_IT_GIT_WORKDIR:-/tmp/mega2-git}` → 容器 `/work`（`working_dir: /work`）。**启栈前应由宿主机预创建且对测试 UID 可写**（推荐 `mkdir -p "$dir" && chmod 1777 "$dir"`）。若缺失，Docker 可能以 `root:root` 自动建目录，导致后续未提权进程 `EACCES`；`./scripts/dev-test.sh up-full` 会在 `--profile git up` 前 mkdir（历史上的 CI `validate-config` 也如此，见「CI 入口」）。该路径跨用例可见；用例只在其下自建**子目录**并清理。相对路径按 compose 文件所在目录（仓库根）解析，与 harness `git_cli_workdir()` 对齐（不以 `bin/` CWD 为准）。**`MEGA2_IT_GIT_WORKDIR` 仅在容器创建时解析**：改根路径必须用同一环境变量值执行 `docker compose -p mega2-it -f docker/docker-compose.test.yml up -d --force-recreate git-cli`（或整栈 recreate）；已在跑的栈上事后 `export` 新值不会改挂载 |
| 运行身份 | `user: "${MEGA2_IT_GIT_UID:-1000}:${MEGA2_IT_GIT_GID:-1000}"`（Compose 可解析的数值默认，不依赖 Bash 未 export 的 `$UID`）。默认 `1000:1000` 对齐常见 CI runner；本地若 `id -u` 不是 1000，启栈前必须 `export MEGA2_IT_GIT_UID=$(id -u) MEGA2_IT_GIT_GID=$(id -g)`。变更 UID/GID 后需要 `--force-recreate git-cli` |
| profiles | `profiles: ["git"]`（**不**参与默认 `up -d --wait`；验收路径显式 `--profile git`） |
| depends_on | 无 |
| 清理 | `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile git down -v`（或整栈 `down -v`）；零残留按 project label 判定 |
| CI 入口 | （历史登记，该 workflow 已不在仓库中）`.github/workflows/config-validation.yml` 的 `validate-config`：mkdir 工作根、导出 `MEGA2_IT_GIT_UID/GID=$(id -u/g)` 后 `--profile git up -d --wait`，再跑 `cargo test -p mega2 --test integration_git_cli -- --test-threads=1`；`.github/workflows/git-protocol-smoke.yml` 在协议路径变更时同样先拉起 `git-cli`，再以 `cargo test -p mega2 --release --test integration_git_cli` 跑同一 target（复用本 job 的 release 构建；补 allowlist A 未含协议路径的覆盖缺口），其后用宿主机 git（pin 见「客户端版本确定性规则」）跑脚本矩阵；该 job `timeout-minutes: 60`（release 构建 + cargo gate + shell smoke） |
| secret | **不**注入任何 secret；凭据由用例经 credential helper / env 注入 |
| 目标 OS / 降级 | **Linux + macOS Docker Desktop**（bridge + `host.docker.internal`）。compose `git-cli`（pin `git version 2.49.1`）是唯一验收 runner。宿主机 `git` 仅当显式 `MEGA2_IT_ALLOW_HOST_GIT=1` 时用于本地实验，且不得冒充固定版本门（例外：`integration_git_lfs` 断言宿主 git-lfs 版本，见上文「本机宿主 git-lfs」） |

### git-smoke（linked 栈级 git 协议 smoke，profile `smoke`）

`git-cli` 与 `git-smoke` 都在默认 bridge 网络上；区别在于 `git-cli` 经 `host.docker.internal` 访问 cargo-native harness 在宿主上自起的服务，而 `git-smoke` 直接对 compose 常驻的 `mega2`（profile `app`）跑 `scripts/git_protocol_smoke.sh` 的 push/pull 矩阵。它把「项目镜像 + 其它镜像 link 在一起」的完整 compose 测试环境落到 git 协议面：ls-remote / clone / fetch / shallow / blobless / push CL / tag-reject / LFS。

git-smoke 固定版本: `git version 2.49.1`（与 git-cli 同基底）

git-smoke runner git-lfs 固定版本: `git-lfs/3.8.0`

| 项 | 值 |
|---|---|
| 服务名 | `git-smoke` |
| 镜像 | `mega2-git-smoke:3.8.0`（本地构建，`build: Dockerfile.git-smoke`）。基底为固定 digest `alpine/git:v2.49.1@sha256:c0280cf9572316299b08544065d3bf35db65043d5e3963982ec50647d2746e26`，叠加 sha256 校验安装的 git-lfs `3.8.0`、`bash`（smoke 脚本需要）、`postgresql-client`（psql 用于 seed access_token）与 `ripgrep`（脚本 ref 断言）；无 `latest`，git pin 不变 |
| 固定版本字符串 | 见上文 `git-smoke 固定版本`（容器内 `git --version` 必须与该字符串完全相等）；`git lfs version` 输出必须以上文 git-lfs pin 前缀开头 |
| 端口 | 无独立端口映射；通过 `networks.default` 访问 `mega2:8000` |
| healthcheck | `CMD-SHELL git --version >/dev/null && git lfs version \| grep -q '^git-lfs/3[.]8[.]0 '`（见 `docker/docker-compose.test.yml`） |
| entrypoint / init | `entrypoint: ["/bin/bash","-c","exec sleep infinity"]`（常驻供 `exec`）；`init: true` |
| 网络 | **加入 `networks.default`**（与 `git-cli` 相同 bridge；`git-smoke` 访问 compose 内 `mega2` DNS 名） |
| 卷 / 工作目录 | 挂载共享宿主路径 `${MEGA2_IT_GIT_WORKDIR:-/tmp/mega2-git}` → 容器 `/work`（`working_dir: /work`）；另以只读挂载 mega2 仓库根 → `/repo`（供 `scripts/git_protocol_smoke.sh` 在容器内执行） |
| 运行身份 | `root`（compose 未设 `user:`，镜像最终为 `USER root`；镜像内另有 `gitsmoke`（uid 1000）passwd 条目供 git / ssh 使用）；`MEGA2_IT_GIT_UID/GID` 对它不起作用 |
| profiles | `profiles: ["smoke"]`（**不**参与默认 `up -d --wait`；须与 `--profile app` 同启，因 `depends_on: mega2`） |
| depends_on | `mega2`（`condition: service_healthy`）——因此 `git-smoke` 必须与 `--profile app` 一起 `up` |
| 环境 | `MEGA2_HTTP_REPO_URL=http://mega2:8000/`（默认指向 linked mega2）；`MEGA2_GIT_SMOKE_PUSH=1`；`MEGA2_GIT_SMOKE_LFS=0`；`MEGA2_IT_SEED_TOKEN`（receive-pack Basic Auth 种子） |
| 清理 | `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app --profile smoke down -v`（或整栈 `down -v`）；零残留按 project label 判定 |
| 运行示例 | 先 `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app --profile smoke up -d --wait`，再在 compose `postgres` 里 seed 一个 access_token（与历史 CI 相同），最后 `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app --profile smoke exec -T git-smoke bash -c 'export MEGA2_HTTP_REPO_URL="http://ci-smoke:<token>@mega2:8000/"; export MEGA2_GIT_SMOKE_PUSH=1; bash /repo/scripts/git_protocol_smoke.sh'` |
| secret | **不**注入任何 secret；token 由 seed 步骤写入 compose `postgres`，经 URL 注入 |
| 目标 OS / 降级 | 跨平台（bridge 网络，非 host 网络）；macOS/Windows Docker Desktop 亦可跑。git pin 与 `git-cli` 一致 |

### mega2（compose 常驻 HTTP，profile `app`）

被测产品进程进入 compose 拓扑的登记条目。**与 cargo 黑盒分层并存**：`bin/tests/integration_*.rs` 仍通过 `CARGO_BIN_EXE_mega2` 按用例拉起隔离进程（唯一端口 / 临时 `MEGA_BASE_DIR` / 隔离 DB）；本服务提供**栈级常驻** `service http`，供 compose smoke、手工联调与 CI 健康探针使用，**不**替代 per-case 隔离门。

| 项 | 值 |
|---|---|
| 服务名 | `mega2` |
| 镜像 | `mega2:local`（`pull_policy: never`）。本地源码构建：`Dockerfile`（context = 含 `mega2/`+`orbit/` 的父目录，bookworm）。CI/快速路径：宿主机 `cargo build -p mega2` 后用 `Dockerfile.it-runtime`（**Ubuntu 24.04**，匹配较新 glibc）打包 `mega2.itbin` |
| 固定版本字符串 | 无客户端 pin；镜像内容随源码 / 宿主机二进制变化，升级为显式 build |
| 端口 | `127.0.0.1:19180:8000`（容器内 CLI 默认 `8000`；仅回环） |
| healthcheck | `curl -f http://127.0.0.1:8000/api/openapi.json` |
| 网络 | 默认 `networks.default` → `mega2-test-network`（可解析 `postgres`/`redis` 服务名） |
| 卷 / 工作目录 | named volume `mega2-data` → `/var/lib/mega2`（`MEGA_BASE_DIR`）；对象存储默认 local → `/var/lib/mega2/objects` |
| profiles | `profiles: ["app"]`：**不**参与默认 `up -d --wait`；显式 `--profile app` |
| depends_on | `postgres`、`redis`（`service_healthy`）。`MEGA_OAUTH__WEBSITE_API_BASE_URL=http://website-next:7001`；`MEGA_OAUTH__ALLOWED_CORS_ORIGINS` 保留既有 IT origin 并含 `http://127.0.0.1:17001`。不声明对 `website-next` 的 `depends_on`：该服务仅属 `web` profile，而 `mega2` 属 `app`；跨 profile 依赖会使 app-only smoke 无法解析。会话联调使用下方规定的 web-first 启动顺序；不依赖 RustFS。 |
| 清理 | `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app down -v`（或与 `--profile git` 一并） |
| CI 入口 | （历史登记，该 workflow 已不在仓库中）`.github/workflows/config-validation.yml`：先检查并 checkout sibling website application checkout，在数据面 `up` 后用 `Dockerfile.it-runtime` 打 `mega2:local`，再以 `--profile app --profile web up -d --wait` 同启服务，探测 `19180/api/openapi.json` 与 `17001/api/auth/get-session`；job 运行 `WEBSITE_IT=1 cargo test -p mega2 --test integration_website_auth -- --test-threads=1`，并在 `if: always()` 带两个 profile `down -v`。 |
| secret | 无注入生产 secret；DB 使用公开测试口令 `mega2_test_password`（用户/库名均为 `mega2`） |
| 降级 / 黑盒 | 栈级 smoke：`integration_compose_mega2_http_smoke`（端口未监听时 soft-skip）。隔离黑盒仍用 `CARGO_BIN_EXE` |

本地源码构建示例：

```bash
# 需 sibling ../orbit
docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app build mega2
docker compose -p mega2-it -f docker/docker-compose.test.yml up -d --wait postgres redis
docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app up -d --wait mega2
curl -sf http://127.0.0.1:19180/api/openapi.json >/dev/null
```

CI / 宿主机二进制打包示例：

```bash
cargo build -p mega2
cp target/debug/mega2 mega2.itbin
docker build -f Dockerfile.it-runtime -t mega2:local .
rm -f mega2.itbin
docker compose -p mega2-it -f docker/docker-compose.test.yml --profile app up -d --wait mega2
```

### scorpiofs（FUSE 工作区守护进程，profile `scorpio`）

ScorpioFS（sibling 仓 `../scorpiofs`）是把 monorepo 路径挂载为本地文件系统的 FUSE
守护进程，只读路径经 mega2 的 `/api/v1/tree`、`/api/v1/tree/content-hash`、
`/api/v1/tree/dir-hash`、`/api/v1/latest-commit`、`/api/v1/file/tree`、
`/api/v1/file/blob/{oid}` 读树与 blob，Antares CL 层另用 `/api/v1/cl/{link}/files-list`
（仅 review policy 提供）。本条目把它接入 compose 拓扑做**栈级联调**：与 `git-smoke`
同型（加入 `networks.default`，直连 compose 常驻 `mega2:8000`），**不**替代任何 cargo
黑盒门，也不是默认门禁。

| 项 | 值 |
|---|---|
| 服务名 | `scorpiofs` |
| 镜像 | `scorpiofs:local`（`pull_policy: never`；本地源码构建 `build: context: ../scorpiofs`，sibling checkout，同外部源码 checkout 惯例）。builder 基底固定 `rust:1.97-slim-bookworm`，runtime `debian:bookworm-slim`（`fuse3` / `libssl3` / `curl`）；无 `latest`。宿主机（Arch 等新 glibc）二进制无法进 bookworm/noble 镜像，不提供 `it-runtime` 式打包 |
| 固定版本字符串 | 无客户端 pin；镜像内容随 `../scorpiofs` 源码变化，升级为显式 `--build`。smoke 把容器内 `scorpio --version` 写入输出 |
| 端口 | `127.0.0.1:12725:2725`（高位 + 仅回环）。ScorpioFS HTTP API **无认证**，禁止改为 `0.0.0.0` 或经反向代理公开 |
| healthcheck | `curl -fsS http://127.0.0.1:2725/health`（`interval 3s` / `retries 40` / `start_period 20s`；`serve` 先挂载 FUSE 工作区再绑定 HTTP，因此 healthy 即代表 FUSE 挂载成功） |
| 特权 | `devices: /dev/fuse`、`cap_add: [SYS_ADMIN, DAC_READ_SEARCH]`、`security_opt: apparmor:unconfined`。`SYS_ADMIN` 为容器内 FUSE `mount(2)` 所需；`DAC_READ_SEARCH` 供 passthrough 层的 `open_by_handle_at(2)`（联调实测：缺少时每次挂载记 `ERROR open_by_handle_at … Operation not permitted` 并回退 fd-backed inodes，功能仍通但偏离真实代码路径）；无 AppArmor 的宿主上 `apparmor:unconfined` 被忽略，Ubuntu（含 GitHub runner）上必需。rootless Docker 不支持 |
| 网络 | 默认 `networks.default` → `mega2-test-network`；容器内经服务 DNS 名访问 `http://mega2:8000` |
| 环境 | `SCORPIO_BASE_URL=http://mega2:8000`、`SCORPIO_LFS_URL=http://mega2:8000/api/v1/lfs`（entrypoint 强制，缺失即启动失败；`lfs_url` 目前只做配置校验）、`SCORPIO_LOG_LEVEL=info`（联调可改 `scorpio=debug`）、`SCORPIO_WORKSPACE=/mnt/scorpiofs/mount`、`SCORPIO_ANTARES_MOUNT_ROOT=/mnt/scorpiofs/antares`（把 FUSE 挂载点移出 named volume，见下）。其余路径由镜像 ENV 指向 `/var/lib/scorpiofs/*` |
| 卷 / 工作目录 | named volume `scorpiofs-data` → `/var/lib/scorpiofs`（store、运行态 `config.toml`、Antares upper / cl / state）。**FUSE 挂载点对宿主可见**：宿主 `${MEGA2_IT_SCORPIO_WORKDIR:-/tmp/mega2-scorpiofs}/mount` 与 `…/antares` 以 `type: bind` + `bind.propagation: rshared` 挂到容器 `/mnt/scorpiofs/mount` / `/mnt/scorpiofs/antares`；容器内的 FUSE 挂载经 peer group 传播回宿主，宿主非 root 用户可直接浏览 `<workdir>/mount` 与 `<workdir>/antares/<mount_id>`（ScorpioFS 以 `allow_other` 挂载）。前提：宿主源路径位于 `shared` 传播的挂载上（systemd 宿主默认如此）、rootful Docker 且 dockerd 未启用 `PrivateMounts`/`MountFlags=slave`。启栈前必须由测试 UID `mkdir -p` 两个目录且 `mount` 为空（`dev-test.sh up-scorpio` 已做；缺失时 Docker 会以 root 自动创建，ScorpioFS 又拒绝非空挂载点）。**挂载点刻意不放在 named volume 之下**：实测把 rshared bind 嵌套在 `/var/lib/scorpiofs` 卷内会把 bind 的副本传播到宿主的卷 `_data` 路径，容器退出后仍残留并令 `docker volume rm` 报 `device or resource busy` |
| stop | `stop_grace_period: 45s`：守护进程收到 SIGTERM 后卸载（daemon join ≤ 20s + Antares cleanup ≤ 15s），卸载事件传播回宿主；若被 SIGKILL，宿主会残留 `Transport endpoint is not connected` 的 FUSE 挂载，需 `sudo umount -l <path>` |
| profiles | `profiles: ["scorpio"]`：**不**参与默认 `up -d --wait`；因 `depends_on: mega2`（profile `app`）必须 `--profile app --profile scorpio` 同启（与 `git-smoke` 同因，避免 `--profile scorpio` 单独激活时依赖未定义） |
| depends_on | `mega2`（`condition: service_healthy`） |
| 清理 | `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile scorpio down -v`（`scripts/dev-test.sh down` 已含 `--profile scorpio`，之后只删除 workdir 下**空**目录，若发现残留 FUSE 挂载则打印 `sudo umount -l` 提示）；零残留按 project label 判定。注意 compose `mega2` 的元数据在共享 `postgres` 的 `public` schema、blob 在 `mega2-data` 卷（local 对象存储）、vault key 也在该卷：**不要单独删 `mega2-data` 卷而保留数据库**，否则重启后 vault 报 `core key file is missing`、blob 读为 0 字节；要么整栈 `down -v`，要么同时 `DROP SCHEMA public CASCADE; CREATE SCHEMA public` |
| 运行示例 | `./scripts/dev-test.sh up-scorpio`（= 准备 workdir + `--profile app --profile scorpio up -d --wait`，首次会构建 `scorpiofs:local`）→ `./scripts/dev-test.sh scorpio-smoke`（= `bash scripts/scorpiofs_smoke.sh`）→ 宿主直接 `ls /tmp/mega2-scorpiofs/mount` |
| smoke | `scripts/scorpiofs_smoke.sh`（宿主 `curl` + `exec -T scorpiofs` + 宿主 `findmnt`；不用 libra）：`GET /health`；dicfuse 只读根含初始化树的 `project` / `third-party`，且 `project/.gitkeep` 内容为 `Placeholder file for /project directory`（见 `docs/manual/monorepo-init.md`）；`host-mount`：宿主 `<workdir>/mount` 是 `fuse` 挂载且以测试 UID 可读同一文件；`POST /api/fs/mount {"path":"project"}` → `GET /api/fs/mpoint` → `POST /api/fs/unmount`；`POST /antares/mounts` → `GET /antares/mounts/{id}/ready` → 容器内与宿主 `<workdir>/antares/<mount_id>` 均列出 `.gitkeep` → `DELETE /antares/mounts/{id}` → 容器内与宿主的该挂载点目录都已不存在（ScorpioFS 自 `remove_mount_dirs` 修复起在成功卸载后回收 mountpoint / upper / cl 目录；旧镜像会在此断言失败）。容器内挂载路径从服务 env（`SCORPIO_WORKSPACE` / `SCORPIO_ANTARES_MOUNT_ROOT`）读取。`127.0.0.1:12725` 未监听且未设 `SCORPIOFS_IT=1` 时输出 `SKIP` 并以 0 退出；宿主无 `findmnt`（macOS）时宿主侧断言记 `SKIP` |
| CI 入口 | 暂无（本地联调）。接入 CI 时须 checkout sibling `../scorpiofs`，`--profile app --profile scorpio up -d --wait` 后运行 smoke，并在 `if: always()` 下带 `--profile scorpio` `down -v`；runner 须提供 `/dev/fuse` |
| secret | 无。ScorpioFS API 无认证、仅回环；不向其注入 mega2 token |
| 目标 OS / 降级 | Linux（rootful Docker）已验收，含宿主可见挂载；macOS Docker Desktop 的 VM 内核带 fuse，`--device /dev/fuse` 理论可用但**未验收**，且 `rshared` 传播止于 VM，宿主 macOS 不会看到挂载（smoke 的宿主侧断言在无 `findmnt` 时 SKIP）。smoke 在服务未启动时 SKIP，不构成默认门禁 |

对照锚点：`docker/docker-compose.test.yml` 的 `scorpiofs` 服务块与 `scorpiofs-data` 卷；
harness 变量 `MEGA2_IT_SCORPIO_WORKDIR`（默认 `/tmp/mega2-scorpiofs`）与 `MEGA2_IT_GIT_WORKDIR` 同规则：
启栈前由测试 UID 创建、变更后须 `--force-recreate scorpiofs`。运行流程见 `docs/development.md`「ScorpioFS 联调」。

### website-next + website-db-init + website-collab（Better Auth IT，profile `web`）

| 项 | 值 |
|---|---|
| 服务名 | `website-db-init`、`website-next`、`website-collab` |
| 镜像 / 构建 | `website-db-init` 与 `website-next` 从 sibling website application checkout 的 `apps/web/Dockerfile` 构建，分别使用 `db-init` 与 `runner` target；`db-init` target 携带 `pnpm` / Drizzle / `pg`。`website-collab` 从 `apps/collab-server/Dockerfile` 构建。三个镜像均为本地 `pull_policy: never` tag；首次或改指 sibling 后必须带 `--build`，避免复用旧镜像。 |
| 端口 | `website-next`：`127.0.0.1:17001:7001`；`website-collab`：`127.0.0.1:17002:7002`（均仅回环） |
| healthcheck | 容器内 Node TCP 连接 `127.0.0.1:7001`；只在 Next 进程已监听时 healthy |
| 网络 | 默认 `networks.default` → `mega2-test-network`，可由后续 `mega2` profile 通过 `website-next:7001` 访问 |
| 账户库 | `website-db-init` 与 `website-next` 使用 `DATABASE_URL=postgresql://mega2:mega2_test_password@postgres:5432/website`。前者先经 `DATABASE_ADMIN_URL` 在共享 `postgres` 上幂等 `CREATE DATABASE website`，再运行 `pnpm exec drizzle-kit migrate`。账户数据与 `mega2` 业务库隔离（ADR-WA-07）；**禁止**把 website URL 指到库名 `mega2`。无 named volume（状态在 Postgres 数据卷） |
| profiles | 两服务均为 `profiles: ["web"]`，不参与默认 `up -d --wait`；显式启动：`docker compose -p mega2-it -f docker/docker-compose.test.yml --profile web up -d --wait website-next` |
| depends_on / 会话联调顺序 | `website-db-init` → `postgres`（`service_healthy`）；`website-collab` 独立健康检查；`website-next` → `postgres` + `website-db-init`（`service_completed_successfully`）+ `website-collab`（`service_healthy`）+ **`rustfs-init`（`service_healthy`，FS-02：确保保留名 `monoui` 桶已建）**；初始化失败时 Next 不会启动。没有 `mega2` 跨 profile 依赖 |
| 对象存储 env（FS-02） | `STORAGE_PROVIDER=s3`，`S3_BUCKET=monoui`，`S3_ENDPOINT=http://rustfs:9000`，`S3_PUBLIC_URL=http://127.0.0.1:19000/monoui`，`S3_FORCE_PATH_STYLE=true`，凭据 `rustfs` / `rustfs_secret`；该 bucket 名为兼容性保留值；若检出 sibling 仓库 website application checkout，实现说明位于 `docs/implementation/workspace-storage-backend.md` |
| 清理 | `docker compose -p mega2-it -f docker/docker-compose.test.yml --profile web down -v`；`-v` 删除 Postgres 数据卷后 `website` 库与 schema 不保留，零残留仍按 compose project label 判定 |
| CI 入口 | （历史登记，该 workflow 已不在仓库中）`.github/workflows/config-validation.yml` 强制 checkout sibling website application checkout。构建 `mega2:local` 后以 `--profile app --profile web up -d --wait` 同启，设置 `WEBSITE_IT=1` 跑 `integration_website_auth`，并在 `if: always()` 使用相同 profile `down -v`。 |
| secret | `BETTER_AUTH_SECRET` 是仅用于本地 IT 的公开固定值；不得替换为或记录生产 secret |
| 邮件 env（website 侧） | website frontend 自管 `EMAIL_PROVIDER=test`（内存记录，无云调用）等；本仓不登记、不启动 SMTP 捕获服务 |
| 性能 | 首次 source build 预算 ≤ 20 分钟；默认 profile 不构建、不启动该服务 |

对照锚点：`docker/docker-compose.test.yml` 的 `website-db-init` / `website-next` 服务块；拓扑语义见
[`website-auth.md`](./website-auth.md) §5。

会话服务连通性 smoke（未登录响应可为 `null`，但不得是连接错误）：

```bash
docker compose -p mega2-it -f docker/docker-compose.test.yml \
  --profile app --profile web exec -T mega2 \
  curl -fsS http://website-next:7001/api/auth/get-session
```

栈级会话黑盒（ITW-03）：

```bash
source .env.test
WEBSITE_IT=1 cargo test -p mega2 --test integration_website_auth -- --test-threads=1 --nocapture
```

未设置 `WEBSITE_IT=1` 时该 target 会明确输出 `SKIP`，供默认数据面循环使用；设置后，
`website-next:17001` 不可达即测试失败，因此 CI 不会将跳过误记为通过。本仓不再提供
产品邮件 IT target。

### cargo-native self-start SSH（ADR-GM-05）

SSH 集成测试**禁止**在测试栈中新增 sshd 服务；唯一拓扑是 cargo-native self-start：
每 case 用 `CARGO_BIN_EXE_mega2 --config CASE/config.toml service ssh --host 127.0.0.1 --ssh-port PORT`
在临时高位端口拉起进程，证据落在 `bin/tests/integration_git_ssh.rs`。

| 项 | 值 |
|---|---|
| Host key | Vault `ssh_server_key` 密文只进 case DB；明文仅服务内存；`MEGA_BASE_DIR=CASE/ssh/base` |
| Client key | `ssh-keygen` → `CASE/ssh/client_ed25519`（mode `0600`） |
| DB seed | `ssh_keys` 插入 fingerprint（`ssh-keygen -lf … -E sha256` 第二列） |
| known_hosts | `ssh-keyscan -p PORT 127.0.0.1` 写入 `CASE/ssh/known_hosts`（仅本 case 端口） |
| GIT_SSH_COMMAND | `ssh -i CASE/ssh/client_ed25519 -o IdentitiesOnly=yes -o UserKnownHostsFile=CASE/ssh/known_hosts -o StrictHostKeyChecking=yes -p PORT` |
| 端口 | 探测 `127.0.0.1:0` 取 ephemeral 端口后立即启动 `--ssh-port` |
| 清理 | SIGINT 停服务、回收 PID、删除临时 DB 与 `CASE/ssh` |

## 强制纪律

**新增服务必须先按 checklist 登记评审，禁止绕规范加服务。**
