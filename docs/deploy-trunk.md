# Trunk / storage-only 部署

本文是 `push_policy=trunk` 与 storage-only HTTP 的运维事实源。产品规则（唯一公开分支、N 分流、不变式、墓碑、索引）见 [`monorepo.md`](./monorepo.md)。设计论证见 [`refactoring/trunk-push.md`](./refactoring/trunk-push.md)。配置键见 `config/config.toml` 与 [`refactoring/config.md`](./refactoring/config.md)。

> **范围**：不接入 mega2 用户系统、不使用 Issue / Change List、不走评审门控的存储与分发部署。默认形态仍是 `push_policy=review`；未改配置的既有部署行为不变。

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

`[monorepo].object_format`：`sha1` 为标准 Git；`sha256` 与 `blake3` 是 **git-internal / Libra extension**，不宣称与标准 Git 客户端互通（[`refactoring/protocol.md`](./refactoring/protocol.md)，[`plan/plan-20260907.md`](./plan/plan-20260907.md)）。Git Object Format 与 LFS Digest 独立：`Git=blake3/LFS=sha256` 与 `Git=sha256/LFS=blake3` 可表达；LFS BLAKE3 业务面属于 `DEFER-B3-LFS-01`。

产品 **API 写**（`POST /api/v1/create-entry`、`POST /api/v1/delete-entry`、`POST /api/v1/move-entry`、`POST /api/v1/edit/save`）在 trunk 下经 **`git.push_auth`** 鉴权后，将对象写入存储并用 **MonoWriteQueue** 前进 path tip（与 `git push` 同 tip 权威；见 [`plan-20260904.md`](./plan/plan-20260904.md)）。`delete-entry` / `move-entry` 在 `is_directory=false` 时可删 / 移文件（省略字段仍只针对目录；见 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)）。成功响应的 `cl_link` 为 `null`，不创建 `mega_cl` / `refs/cl/*`。写后同栈 `git clone` / `git pull` 可读到新内容。集成黑盒见 [`refactoring/integration.md`](./refactoring/integration.md) 的 `integration_api_write_trunk`。monorepo **tag 写**（`POST /api/v1/tags`、`DELETE /api/v1/tags/{name}`；plan-20260917 LB-04 起挂在 storage-only）同样经 `git.push_auth` 鉴权——create 以 `path_context`（缺省 `/`）、delete 固定以 `/` 为鉴权 path：delete 与 root / 缺省 `path_context` 的 create 需要能覆盖 `/` 的 token（`paths` 省略/空 = whole repo），非根 `path_context` 的 create 可用覆盖该 path 的 token——但只写 tag 元数据（`refs/tags/*`；注解 tag 另写 `mega_tag` 行），不经 MonoWriteQueue、不前进 path tip；`POST /api/v1/tags/list` 与 `GET /api/v1/tags/{name}` 不要求 Authorization。

启动期 fail-closed（`Config::validate` / `AppContext::new`）：

1. `trunk` + `cedar.enforcement != "off"` → 拒绝。
2. `trunk` + 仍有 open CL → 拒绝。
3. `push_policy` 变更且 `push_queue` 有非终态行 → 拒绝（须排空/取消）。
4. `push_auth ∈ {token, none}` ⇒ 必须 `push_policy=trunk`。
5. `push_policy=trunk` ⇒ 必须显式 `push_auth`。
6. storage-only（显式 `push_auth`）⇒ 必须 `git.ssh_receive_pack = false`（省略 ≠ 关闭）。
7. 形态切换后执行索引水位重置（第 5 节）。

HTTP 表面：只读 preview + **产品写**（`create-entry` / `delete-entry` / `move-entry` / `edit/save`；目录变更契约见 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)）+ **monorepo tags**（`POST /tags`、`POST /tags/list`、`GET` / `DELETE /tags/{name}`；LB-04）+ Git smart HTTP + **LFS**（`/info/lfs`、`/api/v1/lfs`）；**不**注册 CL / issue / reviewer / OAuth user 路由；OpenAPI（`/api/openapi.json`）列出 LFS 与上述写路径，CL/issue 为空。只读 blob/tree/blame 保留。

## 2. 安全边界（无评审授权 ≠ 无访问控制）

Trunk **没有** review 门控与 Cedar 判定：`cedar.enforcement` 必须为 `off`，授权快照不构建、不被消费。

保留 UN-16 的写入侧钩子（拒绝删除 `refs/heads/main` + notify）**不构成授权保护**。那是结构性保护（快照若将来开启仍以 main 的 `/.mega_cedar.json` 为源），不是评审或 Cedar。

仍然生效的访问控制：

- **`push_auth=token`**：静态 token 常量时间查找；命中后身份为 token 名。`paths` 前缀按**组件边界**授权（`/project/foo` 不授权 `/project/foobar`）。认证身份与 commit author 分离——author 是自声明 provenance，不参与判定。Git receive-pack、**LFS 批/锁写**与 **产品 API 写**共用该模型（见第 6 节与上文 API 写段）。无凭据 / 坏 token → **HTTP 401**；path 越权 → **HTTP 403**。
- **`push_auth=none`**：允许无凭据 API 写（requester=`anonymous`），仅适用于受控网络（第 3 节）。
- **对象存储与 pack 收发**仍按既有存储配置。
- Git 客户端 tag 仍禁止。

## 3. `push_auth=token` 与 `push_auth=none`

凭据经 SecretRef / 文件挂载注入，不要把明文写进提交的 `config.toml`。

```toml
[git]
push_auth = "token"

[[git.push_tokens]]
name = "agent-ci"
token = "${file:/run/secrets/mega2-push-token}"
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

仓库根 `docker-compose-storage-only.yml` 对照 IT 的 `docker-compose.test.yml`，保留 Postgres / Redis / **RustFS** / mega2，挂载 `config/config-storage-only.toml` 与 `/run/secrets/mega2-push-token`（默认 `secrets/mega2-push-token.local`）。不含 website、mailpit、OAuth。

对象存储默认 **RustFS**（`s3compatible`）。**默认启动不要加 `--env-file`**；只有把 mega2 改成本地文件系统后端时，才需要 `--env-file config/compose.env.storage-only.local`（RustFS 容器仍会启动，仅切换 mega2 的 `storage_type`）。

```bash
# 默认 RustFS（s3compatible）——无需 --env-file
docker compose -p mega2-trunk -f docker-compose-storage-only.yml up -d --wait

# 空卷 bootstrap（可复制）
docker compose -p mega2-trunk -f docker-compose-storage-only.yml exec -T mega2 \
  mega2 --config /etc/mega2/config.toml service init --yes

# 干净重跑（破坏性：删 named volume）
docker compose -p mega2-trunk -f docker-compose-storage-only.yml down -v

# 可选：mega2 改用本地文件系统（仅此情况加 --env-file）
docker compose -p mega2-trunk -f docker-compose-storage-only.yml \
  --env-file config/compose.env.storage-only.local up -d --wait

# HTTP http://127.0.0.1:9000/  SSH ssh://git@127.0.0.1:2222/
# RustFS API/console: 127.0.0.1:29000 / 29001
# push token 默认: mega2-storage-only-local-dev-token-0001
```

### 8.1 Compose 黑盒 Git 协议 smoke（`git` 客户端）

Trunk / storage-only 协议冒烟使用 **`scripts/git_protocol_smoke_storage_only.sh`**，经 compose `--profile smoke` 的 `git-smoke` 容器内真实 **`git` / `git-lfs` / `ssh`** 调用已发布端口。这是 **compose 黑盒**，与 cargo `CARGO_BIN_EXE` 黑盒分层并存；**不要**用 `libra` 作协议客户端，也**不要**在 trunk 上跑 review/CL 语义的 `scripts/git_protocol_smoke.sh`。

产品 **API 写 → Git 可见性** 另用 **`scripts/api_write_smoke_storage_only.sh`**（`curl` + `git`；同样禁止 `libra` 客户端）：

```bash
TOKEN='mega2-storage-only-local-dev-token-0001'
docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T \
  -e MEGA2_HTTP_REPO_URL="http://x:${TOKEN}@mega2:8000/" \
  -e MEGA2_API_BASE=http://mega2:8000 \
  -e MEGA2_IT_SEED_TOKEN="${TOKEN}" \
  git-smoke bash /repo/scripts/api_write_smoke_storage_only.sh
```

稳定 case：`API create-entry then git clone sees file`、`API edit/save then git pull sees update`、`API write rejects unauthenticated`；plan-20260917 LB-05 追加 `delete-entry-git-visible`、`move-entry-git-visible`、`tags-list-create-delete`、`delete-entry-unauth-401`（目录变更后 `git clone` / `git pull` 工作树与 `GET /tree` 一致；tag 用 token create / delete、匿名 list / get）。单 case 用 `MEGA2_SMOKE_CASE=<精确名>`（未匹配退出 2），`-h` 列出全部 case 名。summary 须 `0 failed`——全量运行请在干净栈上（`down -v` 重建 + `service init --yes`）执行，或用 `MEGA2_SMOKE_CASE` 只选所需 case：AW-04 的 `API create-entry then git clone sees file` 用固定文件名，在同一栈上第二次全量运行会撞重名 500 而 `failed=1`（LB-05 的四个 case 用每次运行唯一的名字）。可被 plan-20260906 吸收为附加 case（见 `docs/refactoring/test-infra.md`）。矩阵行见 [`refactoring/integration.md`](./refactoring/integration.md) 的 `integration_api_write_trunk`。

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T \
  -e MEGA2_HTTP_REPO_URL=http://mega2:8000/ \
  git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh
```

单 case：`MEGA2_SMOKE_CASE='HTTP ls-remote'`（精确 case 名；未匹配退出非 0）。已注册只读 case：`HTTP ls-remote`、`HTTP clone`、`HTTP fetch`、`HTTP protocol v2 fetch`、`HTTP shallow clone depth=1`、`HTTP protocol v2 ls-remote`、`HTTP protocol v2 blob:none clone`。写 case（需 `MEGA2_GIT_SMOKE_PUSH=1` + token / `MEGA2_IT_SEED_TOKEN`）：`HTTP trunk push`——对 **`/project`**（非根 `/`；B0 拒根路径）断言 tip 前进；不以新建 `refs/cl` 为成功条件。根 URL 入参时脚本自动改写为 `/project`。另：`HTTP reject Git-client tag push`（tag push 非 0 且远端无残留 tag）。负例（默认 token 栈、**无凭据** URL）：`HTTP reject unauthenticated push`。

Token 写示例：

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T \
  -e MEGA2_HTTP_REPO_URL=http://mega2:8000/ \
  -e MEGA2_IT_SEED_TOKEN=mega2-storage-only-local-dev-token-0001 \
  -e MEGA2_GIT_SMOKE_PUSH=1 \
  -e MEGA2_SMOKE_CASE='HTTP trunk push' \
  git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh
```

`push_auth=none` 栈（ADR-SO-06）：双 `-f` 覆盖挂载 `config/config-storage-only.none.toml`，并 `--force-recreate mega2`。opt-in case：`HTTP trunk push (none)`（无凭据 tip 前进）、`HTTP reject Git-client tag push (none)`（tag push 非 0 且远端无残留）。

```bash
docker compose -p mega2-trunk \
  -f docker-compose-storage-only.yml \
  -f docker-compose-storage-only.auth-none.yml \
  up -d --wait --force-recreate mega2

docker compose -p mega2-trunk \
  -f docker-compose-storage-only.yml \
  -f docker-compose-storage-only.auth-none.yml \
  --profile smoke \
  exec -T \
  -e MEGA2_HTTP_REPO_URL=http://mega2:8000/ \
  -e MEGA2_GIT_SMOKE_PUSH=1 \
  -e MEGA2_SMOKE_CASE='HTTP trunk push (none)' \
  git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh
```

切回默认 token 样例：去掉第二个 `-f`，再 `--force-recreate mega2`。

LFS 网内 URL（ADR-SO-04）：`--env-file config/compose.env.storage-only.lfs-innetwork` 把 `MEGA_HTTP__PUBLIC_BASE_URL` / `MEGA_LFS__SSH__HTTP_URL` 设为 `http://mega2:8000`，供 `git-smoke` 容器内 LFS href 可达。opt-in case：`HTTP LFS push and pull (trunk)`（需 `MEGA2_GIT_SMOKE_PUSH=1` + `MEGA2_GIT_SMOKE_LFS=1` + token）。

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml \
  --env-file config/compose.env.storage-only.lfs-innetwork \
  up -d --wait --force-recreate mega2

docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T \
  -e MEGA2_HTTP_REPO_URL=http://mega2:8000/ \
  -e MEGA2_IT_SEED_TOKEN=mega2-storage-only-local-dev-token-0001 \
  -e MEGA2_GIT_SMOKE_PUSH=1 \
  -e MEGA2_GIT_SMOKE_LFS=1 \
  -e MEGA2_SMOKE_CASE='HTTP LFS push and pull (trunk)' \
  git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh
```

切回宿主机 LFS URL：去掉 `--env-file`，再 `--force-recreate mega2`。

SSH 只读（`anonymous_access=true` → `auth_none`，**不**依赖 UserStorage 公钥）。首次连接写入 known_hosts：

```bash
# Inside git-smoke (or any OpenSSH client on the compose network):
export GIT_SSH_COMMAND='ssh -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/tmp/mega2-smoke-known_hosts'
# Or omit GIT_SSH_COMMAND: scripts/git_protocol_smoke_storage_only.sh defaults the same
# StrictHostKeyChecking=accept-new + a workdir UserKnownHostsFile.
```

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T \
  -e MEGA2_SSH_REPO_URL=ssh://git@mega2:2222/ \
  -e MEGA2_SMOKE_CASE='SSH ls-remote' \
  git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh
```

已注册 SSH case：`SSH ls-remote`、`SSH clone`、`SSH fetch`、`SSH protocol v2 fetch`、`SSH shallow clone depth=1`、`SSH protocol v2 ls-remote`、`SSH protocol v2 blob:none clone`、`SSH reject receive-pack`（未设 `MEGA2_SSH_REPO_URL` 时后者 SKIP）。

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T \
  -e MEGA2_SSH_REPO_URL=ssh://git@mega2:2222/ \
  -e MEGA2_SMOKE_CASE='SSH reject receive-pack' \
  git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh
```

结果获取（stdout 为主；推荐宿主 tee）：

```bash
mkdir -p target/tmp
LOG="target/tmp/so-smoke-$(date -u +%Y%m%dT%H%M%SZ)-${MEGA2_SMOKE_CASE:-all}.log"
set -o pipefail
docker compose -p mega2-trunk -f docker-compose-storage-only.yml --profile smoke \
  exec -T git-smoke bash /repo/scripts/git_protocol_smoke_storage_only.sh \
  2>&1 | tee "$LOG"
```

排障工作区（非 PASS 主证据）：宿主 `${MEGA2_IT_GIT_WORKDIR:-/tmp/mega2-trunk-git}`（挂载为容器 `/work`）。

与 `-p mega2-it` 的测试栈可并存（端口 25432 / 26379 / 9000 / 2222 / 29000）。生产凭据用 `MEGA2_PUSH_TOKEN_FILE` 覆盖，勿提交。

## 9. Agent 工作流备忘

每次 trunk 推送后执行：

```bash
git fetch && git reset --hard origin/main
```

N = 1 时为 no-op；N > 1 时对齐到 squash tip。子路径 clone（例如 `/project/foo`），不要对 `/` 做根 clone 作为该形态的假设。

## 10. storage-only OCI Distribution（`/v2`）

架构与端点事实源：[`refactoring/oci.md`](./refactoring/oci.md)（plan-20260902）。本节只覆盖运维启用与 `docker login` 认证语义。

### 10.1 启用条件

`/v2` **双重门**：必须同时满足

1. storage-only：配置显式 `git.push_auth`（`token` 或 `none`）——与第 3 节相同；
2. `[oci] enabled = true`。

缺一则**不挂载** `/v2`（裸 404）。`enabled=true` 且非 storage-only → **启动拒绝**。`config/config-storage-only.toml` 样例已含启用段；review / 省略 `push_auth` 的形态不得打开本开关。

### 10.2 `docker login` 与推送

认证复用 `[[git.push_tokens]]`（无独立 OCI token 服务）：

```bash
# username 可为任意值；password = push token 密文
docker login <host> -u oci -p '<push-token>'
docker tag <local-image> <host>/<repo>:<tag>   # repo 可为多段，如 team/app
docker push <host>/<repo>:<tag>
docker pull <host>/<repo>:<tag>
```

HTTP 明文 registry（如本机 compose `http://127.0.0.1:9000`）需在客户端配置 insecure registry；生产应终止 TLS。

宿主机 live smoke：`bash scripts/oci_smoke_storage_only.sh`（默认 registry `http://127.0.0.1:9000`；token 见 `MEGA2_OCI_SMOKE_TOKEN` 或 `secrets/mega2-push-token.local`）。docker / 端点不可达时脚本 SKIP 并以退出码 0 结束。

### 10.3 认证语义摘要

| 面 | 行为 |
|---|---|
| 写（push / upload） | `push_auth=token`：Basic/Bearer 取 token，且 `paths` 必须覆盖 `"/" + repo`；否则 `DENIED`。`push_auth=none`：写全放行（同第 3 节受控网络前提）。 |
| 读（pull / ping） | 跟随 `git.anonymous_access`：开 → 无凭据可读；关 → 需有效 token。无效凭据恒 401（含 `GET /v2/`，保证 `docker login` 失败可呈现）。 |
| 跨仓 mount | 目标写授权 + 源读授权。 |

manifest/blob `DELETE` 路由存在但恒返回 OCI `UNSUPPORTED`（405）。`_catalog` / referrers 未实现。
