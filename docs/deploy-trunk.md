# Trunk / storage-only 部署

本文是 `push_policy=trunk` 与 storage-only HTTP 的运维事实源。产品规则（唯一公开分支、N 分流、不变式、墓碑、索引）见 [`monorepo.md`](./monorepo.md)。设计论证见 [`refactoring/trunk-push.md`](./refactoring/trunk-push.md)。配置键见 `config/config.toml` 与 [`refactoring/config.md`](./refactoring/config.md)。

> **范围**：不接入 monoengine 用户系统、不使用 Issue / Change List、不走评审门控的存储与分发部署。默认形态仍是 `push_policy=review`；未改配置的既有部署行为不变。

## 1. `push_policy=trunk`

```toml
[monorepo]
push_policy = "trunk"
max_push_commits = 250   # trunk 链长；review 仍用 MAX_CL_CHAIN_COMMITS = 250
merge_writer = "queue"   # 生产稳态；缺省 legacy 仅迁移回退

[git]
push_auth = "token"      # 或 "none"；缺省（省略）拒绝启动
ssh_receive_pack = false # storage-only 必须显式写 false，省略会拒绝启动
# [[git.push_tokens]] 见第 3 节
```

启动期 fail-closed（`Config::validate` / `AppContext::new`）：

1. `trunk` + `cedar.enforcement != "off"` → 拒绝。
2. `trunk` + 仍有 open CL → 拒绝。
3. `push_policy` 变更且 `push_queue` 有非终态行 → 拒绝（须排空/取消）。
4. `push_auth ∈ {token, none}` ⇒ 必须 `push_policy=trunk`。
5. `push_policy=trunk` ⇒ 必须显式 `push_auth`。
6. storage-only（显式 `push_auth`）⇒ 必须 `git.ssh_receive_pack = false`（省略 ≠ 关闭）。
7. 形态切换后执行索引水位重置（第 5 节）。

HTTP 表面：只读 preview + Git smart HTTP；**不**注册 CL / issue / reviewer / code_edit 写路由；OpenAPI（`/api/openapi.json`）如实为空。只读 blob/tree/blame 保留。

## 2. 安全边界（无评审授权 ≠ 无访问控制）

Trunk **没有** review 门控与 Cedar 判定：`cedar.enforcement` 必须为 `off`，授权快照不构建、不被消费。

保留 UN-16 的写入侧钩子（拒绝删除 `refs/heads/main` + notify）**不构成授权保护**。那是结构性保护（快照若将来开启仍以 main 的 `/.mega_cedar.json` 为源），不是评审或 Cedar。

仍然生效的访问控制：

- **`push_auth=token`**：静态 token 常量时间查找；命中后身份为 token 名。`paths` 前缀按**组件边界**授权（`/project/foo` 不授权 `/project/foobar`）。认证身份与 commit author 分离——author 是自声明 provenance，不参与判定。
- **对象存储与 pack 收发**仍按既有存储配置。
- **LFS 不可用**（第 6 节）。Git 客户端 tag 仍禁止。

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

`push_auth=none` **必须写在配置里**（省略 ≠ none）。它旁路 git HTTP 的 token/OAuth 前置，只适用于**受控内网、回环或 Unix socket 前置**的部署。对公网暴露 `none` 等于匿名 receive-pack。启动会打可诊断警告；SSH receive-pack 在 storage-only 下仍关闭（第 4 节），因此「无凭据推送」只可能出现在你显式打开的 HTTP 面上。

## 4. SSH

Storage-only（显式 `push_auth`）**不暴露 SSH receive-pack**。启动要求配置里写 `ssh_receive_pack = false`；省略该项会 fail-closed，不是默默关闭。Clone/fetch 若走 SSH，仍受该键约束。需要推送时用 Git smart HTTP + token（或 `none` + 网络边界）。

## 5. 形态切换

| 方向 | 前置 | 索引 |
|---|---|---|
| `review` → `trunk` | 无 open CL；排空 `push_queue` 非终态行；`cedar.enforcement=off`；显式 `push_auth` | `UPDATE blob_paths SET indexed_push_id = NULL`（`RESET INDEX WATERMARK`） |
| `trunk` → `review` | 同样排空非终态行 | 必须重置水位，否则 review 只写 `IS NULL` 行会使已有水位永久失效 |

`trunk` → `review` **不**重建 roll-up 之前的 CL 历史。切回后新的推送重新走 CL 管线。

## 6. LFS 仅 review 形态

LFS 批/锁/上传走 `UserStorage` 认证，静态 token 模型不覆盖。Trunk 不挂载 `/info/lfs` 与 `/api/v1/lfs`；若仍命中 handler，返回 404 `push_policy=trunk: LFS is review-only`。需要 LFS 时使用 `push_policy=review`，或另立 token-aware LFS 授权议题。

## 7. 阶段 5 多 token 运维（DEFER-TP-05）

基础认证（单/多 token 查找、组件边界前缀、`none` 旁路）已随阶段 4.2a（TP-19/20）交付。Token 轮换、审计、限流等运维强化**未**规范，登记为 `DEFER-TP-05`（重启条件：`trunk-push.md` 阶段 5 补齐可执行规范）。

## 8. Agent 工作流备忘

每次 trunk 推送后执行：

```bash
git fetch && git reset --hard origin/main
```

N = 1 时为 no-op；N > 1 时对齐到 squash tip。子路径 clone（例如 `/project/foo`），不要对 `/` 做根 clone 作为该形态的假设。
