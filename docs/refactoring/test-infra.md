# 测试基建规范

本文是 monoengine 测试基建的**单一事实源**：双层测试职责、fixture 生命周期、
docker-compose 测试栈新服务登记 checklist、客户端版本确定性规则。
既有的 minio + minio-init 扩展在此按 checklist 补登记；后续新服务（含 git-cli
runner）必须先按本节登记并评审，**禁止绕规范直接改 `docker-compose.test.yml`**。

策略与覆盖矩阵仍住在 [`integration.md`](./integration.md)；本文不复制那些内容。

## 双层测试职责

### 模块集成测试（crate 内部）

- **位置：** `src/**/*.rs` 中的 `#[cfg(test)] mod tests`（随 `monoengine-core` 编译）。
- **用途：** 需要直接调用 crate 内部 API 的路径（storage、migration、dispatcher、
  触发器等）。可连接 Docker PostgreSQL / Redis / Mailpit，但必须由用例显式配置。
- **边界：** 允许 `use crate::...`；不拉起真实 `monoengine` 二进制。

### 黑盒进程测试（cargo-native）

- **位置：** `bin/tests/integration_*.rs`（属于 `monoengine` 二进制 crate）。
- **用途：** CLI、启动顺序、HTTP / Git 协议 smoke、secret 不回显、进程退出码。
- **边界：** 通过 `CARGO_BIN_EXE_monoengine` 与外部协议断言；**不**导入
  `crate::...`。共享编排 helper 放在 `bin/tests/common/mod.rs`。

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
   `${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-it-git}`。启栈前应：
   `mkdir -p "$dir" && chmod 1777 "$dir"`（保证测试 UID 可写）。若目录缺失，
   Docker bind 可能以 `root:root` 自动创建，随后未提权的 cargo 进程分配子目录
   会 `EACCES`——因此本地与 CI 启动脚本都必须先 mkdir（workflow 侧由 IT-04
   补齐；在此之前勿依赖自动创建）。用例只在该根下分配子目录并清理。变更
   `MONOENGINE_IT_GIT_WORKDIR` 后必须 `--force-recreate git-cli`（或整栈），
   否则 host 与容器挂载会静默分叉。`git-cli` 在 `profiles: ["git"]` 下，默认
   `up -d --wait` **不会**启动它（避免 macOS/Windows Docker Desktop 因
   `network_mode: host` 拖垮既有数据面服务）；需要时加 `--profile git`。
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
9. **非 Linux 本地环境的降级路径**（例如 macOS Docker Desktop 无 `network_mode: host`
   时的宿主机客户端降级，并记录版本）。

另需在登记条目中写明：网络模式（与 `networks.default` 的互斥约束）、卷/工作目录约定、
`profiles` / `depends_on` 语义、CI 入口（若有）。

## 客户端版本确定性规则

- 通过 compose 提供的客户端（如 git-cli runner）必须在登记条目中写明**固定版本字符串**，
  并用 Verification 断言容器内 `git --version`（及同类工具）等于该 pin。
- CI 若改用宿主机客户端，必须 pin 安装版本并在 job summary 输出实际版本（由后续
  任务卡落地，不在本文预登记具体 pin 值）。
- 升级镜像 tag / digest 或 pin 值必须是显式 PR 动作，并同步更新登记条目。

## 已登记服务

### minio + minio-init（既有扩展补登记）

| 项 | 值 |
|---|---|
| 服务名 | `minio`、`minio-init` |
| 镜像 | `minio/minio:RELEASE.2025-04-22T22-12-26Z`；`minio/mc:RELEASE.2025-04-16T18-13-26Z`（固定 tag，非 `latest`） |
| 端口 | `127.0.0.1:19000:9000`、`127.0.0.1:19001:9001`（高位 + 仅回环） |
| healthcheck | `minio`：`curl -f http://localhost:9000/minio/health/live`（见 `docker-compose.test.yml`） |
| 网络 | 默认 `networks.default` → `monoengine-test-net` |
| 卷 / 工作目录 | `minio` 使用容器内路径 `/data`，**无**宿主机 bind-mount；数据仅存在于该容器可写层，`down -v` 后不保留。`minio-init` 无持久卷（one-shot） |
| profiles | `minio-init` 使用 `profiles: ["init"]`：**one-shot** 桶初始化，不参与默认 `up -d --wait`（`--wait` 要求服务保持 running/healthy）；显式执行 `docker compose -p monoengine-it -f docker-compose.test.yml --profile init run --rm minio-init` |
| depends_on | `minio-init` → `minio` 且 `condition: service_healthy` |
| 清理 | `docker compose -p monoengine-it -f docker-compose.test.yml down -v`；零残留按 project label 判定 |
| CI 入口 | `.github/workflows/config-validation.yml` 的 `validate-config` job：先 `docker compose -f docker-compose.test.yml up -d --wait` 拉起含 minio 的栈，再 `--profile init run --rm minio-init` 建桶；job 末尾 `if: always()` 下 `down -v`。当前步骤尚未统一 `-p monoengine-it`（与上文「历史命令」同口径，由后续 CI 任务卡对齐） |
| secret | 公开测试口令 `minioadmin` / `minioadmin`（仅测试栈）；CI 对同类凭据使用 `::add-mask::` |
| 降级 | 无客户端版本 pin 需求；本地可不启 minio（相关 gate 自行 skip/opt-in） |

对照锚点：`docker-compose.test.yml` 的 `minio` / `minio-init` 服务块与 `networks.default`。

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
| 卷 / 工作目录 | 挂载共享宿主路径 `${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-it-git}` → 容器 `/work`（`working_dir: /work`）。**启栈前应由宿主机预创建且对测试 UID 可写**（推荐 `mkdir -p "$dir" && chmod 1777 "$dir"`）。若缺失，Docker 可能以 `root:root` 自动建目录，导致后续未提权进程 `EACCES`；CI `up` 路径在 IT-04 前尚未 mkdir，运维/本地不得依赖自动创建。该路径跨用例可见；用例只在其下自建**子目录**并清理。**`MONOENGINE_IT_GIT_WORKDIR` 仅在容器创建时解析**：改根路径必须用同一环境变量值执行 `docker compose -p monoengine-it -f docker-compose.test.yml up -d --force-recreate git-cli`（或整栈 recreate）；已在跑的栈上事后 `export` 新值不会改挂载 |
| 运行身份 | `user: "${MONOENGINE_IT_GIT_UID:-1000}:${MONOENGINE_IT_GIT_GID:-1000}"`（Compose 可解析的数值默认，不依赖 Bash 未 export 的 `$UID`）。默认 `1000:1000` 对齐常见 CI runner；本地若 `id -u` 不是 1000，启栈前必须 `export MONOENGINE_IT_GIT_UID=$(id -u) MONOENGINE_IT_GIT_GID=$(id -g)`。变更 UID/GID 后需要 `--force-recreate git-cli` |
| profiles | `profiles: ["git"]`（**不**参与默认 `up -d --wait`，以免 host 网络在 Docker Desktop 上拖垮数据面；显式 `--profile git`） |
| depends_on | 无 |
| 清理 | `docker compose -p monoengine-it -f docker-compose.test.yml --profile git down -v`（或整栈 `down -v`）；零残留按 project label 判定 |
| CI 入口 | 由后续 CI 任务卡（IT-04 / IT-11）接入；本卡只登记服务本身 |
| secret | **不**注入任何 secret；凭据由用例经 credential helper / env 注入 |
| 降级 | 非 Linux（如 macOS Docker Desktop 无 host 网络）**不要**启 `--profile git`，降级为宿主机 `git`，用例输出须记录 `git --version` |

## 强制纪律

**新增服务必须先按 checklist 登记评审，禁止绕规范加服务。**
