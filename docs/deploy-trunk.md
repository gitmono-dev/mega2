# Trunk / storage-only 部署

本文是 `push_policy=trunk` 与 storage-only HTTP 的运维事实源。产品规则（唯一公开分支、N 分流、不变式、墓碑、索引）见 [`monorepo.md`](./monorepo.md)。设计论证见 [`refactoring/trunk-push.md`](./refactoring/trunk-push.md)。配置键见 `config/config.toml` 与 [`refactoring/config.md`](./refactoring/config.md)。

> **范围**：不接入 monoengine 用户系统、不使用 Issue / Change List、不走评审门控的存储与分发部署。默认形态仍是 `push_policy=review`；未改配置的既有部署行为不变。

## 1. `push_policy=trunk`

```toml
[monorepo]
push_policy = "trunk"
max_push_commits = 250   # trunk 链长；review 仍用 MAX_CL_CHAIN_COMMITS = 250

[git]
push_auth = "token"      # 或 "none"；缺省（省略）拒绝启动
ssh_receive_pack = false # storage-only 必须显式写 false，省略会拒绝启动
# [[git.push_tokens]] 见第 3 节
```

CL merge 只经 MonoWriteQueue；`[monorepo]` 无 `merge_writer` 键（[`plan-20260910.md`](./plan/plan-20260910.md)）。残留该键按未知字段拒绝启动。

启动期 fail-closed（`Config::validate` / `AppContext::new`）：

1. `trunk` + `cedar.enforcement != "off"` → 拒绝。
2. `trunk` + 仍有 open CL → 拒绝。
3. `push_policy` 变更且 `push_queue` 有非终态行 → 拒绝（须排空/取消）。
4. `push_auth ∈ {token, none}` ⇒ 必须 `push_policy=trunk`。
5. `push_policy=trunk` ⇒ 必须显式 `push_auth`。
6. storage-only（显式 `push_auth`）⇒ 必须 `git.ssh_receive_pack = false`（省略 ≠ 关闭）。
7. 形态切换后执行索引水位重置（第 5 节）。

HTTP 表面：只读 preview + Git smart HTTP + **LFS**（`/info/lfs`、`/api/v1/lfs`）；**不**注册 CL / issue / reviewer / code_edit 写路由；OpenAPI（`/api/openapi.json`）如实为空（CL/issue）并列出 LFS。只读 blob/tree/blame 保留。

## 2. 安全边界（无评审授权 ≠ 无访问控制）

Trunk **没有** review 门控与 Cedar 判定：`cedar.enforcement` 必须为 `off`，授权快照不构建、不被消费。

保留 UN-16 的写入侧钩子（拒绝删除 `refs/heads/main` + notify）**不构成授权保护**。那是结构性保护（快照若将来开启仍以 main 的 `/.mega_cedar.json` 为源），不是评审或 Cedar。

仍然生效的访问控制：

- **`push_auth=token`**：静态 token 常量时间查找；命中后身份为 token 名。`paths` 前缀按**组件边界**授权（`/project/foo` 不授权 `/project/foobar`）。认证身份与 commit author 分离——author 是自声明 provenance，不参与判定。Git receive-pack 与 **LFS 批/锁写**共用该模型（见第 6 节）。
- **对象存储与 pack 收发**仍按既有存储配置。
- Git 客户端 tag 仍禁止。

## 3. `push_auth=token` 与 `push_auth=none`

凭据经 SecretRef / 文件挂载注入，不要把明文写进提交的 `config.toml`。

```toml
[git]
push_auth = "token"

[[git.push_tokens]]
name = "agent-ci"
token = "${file:/run/secrets/monoengine-push-token}"
paths = ["/project"]   # 省略或空 = 全库
```

`push_auth=none` **必须写在配置里**（省略 ≠ none）。它旁路 git HTTP 的 token/OAuth 前置，只适用于**受控内网、回环或 Unix socket 前置**的部署。对公网暴露 `none` 等于匿名 receive-pack **以及匿名 LFS 上传**（批/锁写与 receive-pack 同级）。启动会打可诊断警告；SSH receive-pack 在 storage-only 下仍关闭（第 4 节），因此「无凭据推送 / 无凭据 LFS 写」只可能出现在你显式打开的 HTTP 面上。

## 4. SSH

Storage-only（显式 `push_auth`）**不暴露 SSH receive-pack**。启动要求配置里写 `ssh_receive_pack = false`；省略该项会 fail-closed，不是默默关闭。需要推送时用 Git smart HTTP + token（或 `none` + 网络边界）。

`ssh_receive_pack = false` **不关闭** SSH clone/fetch/pull（upload-pack）。storage-only 读与 HTTP 共用 `git.anonymous_access`：

- `anonymous_access = true`：SSH `auth_none` 放行，无需 UserStorage 公钥、无需 password。
- `push_auth = "none"` 且 `anonymous_access = false`：SSH 读 fail-closed（无 password 通道；勿在 trunk+none 依赖 UserStorage）。
- `push_auth = "token"` 且 `anonymous_access = false`：SSH `auth_password`，password 字段 = `[[git.push_tokens]]` 密文（与 HTTP Basic 只取 password、忽略 username 对齐）。客户端用 `SSH_ASKPASS` / `sshpass` / 定制 `GIT_SSH_COMMAND`。storage-only 仍不提供 SSH receive-pack。

review 形态（省略 `push_auth`）仍走 UserStorage 公钥；默认匿名开时 **不** 成功放行 `auth_none`，以免 OpenSSH 跳过公钥推送。

## 5. 形态切换

| 方向 | 前置 | 索引 |
|---|---|---|
| `review` → `trunk` | 无 open CL；排空 `push_queue` 非终态行；`cedar.enforcement=off`；显式 `push_auth` | `UPDATE blob_paths SET indexed_push_id = NULL`（`RESET INDEX WATERMARK`） |
| `trunk` → `review` | 同样排空非终态行 | 必须重置水位，否则 review 只写 `IS NULL` 行会使已有水位永久失效 |

`trunk` → `review` **不**重建 roll-up 之前的 CL 历史。切回后新的推送重新走 CL 管线。

## 6. LFS 随 `push_auth`

Trunk / storage-only **挂载** `/info/lfs` 与 `/api/v1/lfs`。LFS 批/锁写授权与 Git receive-pack 共用 `git.push_auth`（[`plan-20260909.md`](./plan/plan-20260909.md) ADR-LF-01；**supersede** plan-20260905 TP-18「关闭 LFS」产品决策，不回改该卡历史验收）：

| `push_auth` | LFS 读 | LFS 写（batch upload / locks） |
|---|---|---|
| `none` | 匿名允许 | 匿名允许（与匿名 receive-pack **同级网络边界**；见第 3 节警告） |
| `token` | `anonymous_access` 或有效 token | 有效 `[[git.push_tokens]]` 且 `paths` 覆盖 `LfsRepoContext` 路径 |

Object PUT/GET 仍走 batch 注册后的能力 URL，不逐请求鉴权。Review 形态（省略 `push_auth`）仍走 `UserStorage` mono access token。SSH `git-lfs-transfer` 不在 storage-only 交付范围；LFS 走 HTTP。

不再把 token-aware LFS 列为未决议题。多 token 轮换/审计/限流仍属 `DEFER-TP-05`（第 7 节），与 LFS 是否可用无关。

## 7. 阶段 5 多 token 运维（DEFER-TP-05）

基础认证（单/多 token 查找、组件边界前缀、`none` 旁路）已随阶段 4.2a（TP-19/20）交付。Token 轮换、审计、限流等运维强化**未**规范，登记为 `DEFER-TP-05`（重启条件：`trunk-push.md` 阶段 5 补齐可执行规范）。

## 8. 本地 Compose 栈

仓库根 `docker-compose-storage-only.yml` 对照 IT 的 `docker-compose.test.yml`，保留 Postgres / Redis / **RustFS** / monoengine，挂载 `config/config-storage-only.toml` 与 `/run/secrets/monoengine-push-token`（默认 `secrets/monoengine-push-token.local`）。不含 website、mailpit、OAuth。

对象存储默认 **RustFS**（`s3compatible`）。**默认启动不要加 `--env-file`**；只有把 monoengine 改成本地文件系统后端时，才需要 `--env-file config/compose.env.storage-only.local`（RustFS 容器仍会启动，仅切换 monoengine 的 `storage_type`）。

```bash
# 默认 RustFS（s3compatible）——无需 --env-file
docker compose -p monoengine-trunk -f docker-compose-storage-only.yml up -d --wait

# 空卷 bootstrap（可复制）
docker compose -p monoengine-trunk -f docker-compose-storage-only.yml exec -T monoengine \
  monoengine --config /etc/monoengine/config.toml service init --yes

# 干净重跑（破坏性：删 named volume）
docker compose -p monoengine-trunk -f docker-compose-storage-only.yml down -v

# 可选：monoengine 改用本地文件系统（仅此情况加 --env-file）
docker compose -p monoengine-trunk -f docker-compose-storage-only.yml \
  --env-file config/compose.env.storage-only.local up -d --wait

# HTTP http://127.0.0.1:9000/  SSH ssh://git@127.0.0.1:2222/
# RustFS API/console: 127.0.0.1:29000 / 29001
# push token 默认: monoengine-storage-only-local-dev-token-0001
```

### 8.1 Compose 黑盒 Git 协议 smoke（`git` 客户端）

Trunk / storage-only 协议冒烟使用 **`scripts/git_protocol_smoke_storage_only.sh`**，经 compose `--profile smoke` 的 `git-smoke` 容器内真实 **`git` / `git-lfs` / `ssh`** 调用已发布端口。这是 **compose 黑盒**，与 cargo `CARGO_BIN_EXE` 黑盒分层并存；**不要**用 `libra` 作协议客户端，也**不要**在 trunk 上跑 review/CL 语义的 `scripts/git_protocol_smoke.sh`。

```bash
docker compose -p monoengine-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T \
  -e MONOENGINE_HTTP_REPO_URL=http://monoengine:8000/ \
  git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh
```

单 case：`MONOENGINE_SMOKE_CASE='HTTP ls-remote'`（精确 case 名；未匹配退出非 0）。已注册只读 case：`HTTP ls-remote`、`HTTP clone`、`HTTP fetch`、`HTTP protocol v2 fetch`、`HTTP shallow clone depth=1`、`HTTP protocol v2 ls-remote`（后续卡继续追加）。

结果获取（stdout 为主；推荐宿主 tee）：

```bash
mkdir -p target/tmp
LOG="target/tmp/so-smoke-$(date -u +%Y%m%dT%H%M%SZ)-${MONOENGINE_SMOKE_CASE:-all}.log"
set -o pipefail
docker compose -p monoengine-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh \
  2>&1 | tee "$LOG"
```

排障工作区（非 PASS 主证据）：宿主 `${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-trunk-git}`（挂载为容器 `/work`）。

与 `-p monoengine-it` 的测试栈可并存（端口 25432 / 26379 / 9000 / 2222 / 29000）。生产凭据用 `MONOENGINE_PUSH_TOKEN_FILE` 覆盖，勿提交。

## 9. Agent 工作流备忘

每次 trunk 推送后执行：

```bash
git fetch && git reset --hard origin/main
```

N = 1 时为 no-op；N > 1 时对齐到 squash tip。子路径 clone（例如 `/project/foo`），不要对 `/` 做根 clone 作为该形态的假设。
