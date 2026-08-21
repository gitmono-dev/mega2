# 认证与权限管理

本文面向运维与平台所有者，说明 **website 负责认证、monoengine 负责授权** 的边界与运维口径。开发契约细节见 [`../refactoring/website-auth.md`](../refactoring/website-auth.md) 与 [`../refactoring/contract.md`](../refactoring/contract.md)；Kill Switch 脚本实现细节见 [`../refactoring/integration.md`](../refactoring/integration.md)。

术语约定：**认证** = website Better Auth 会话与 Mono access token / Bot / SSH 公钥身份；**授权** = monoengine Cedar 三态（`off` / `shadow` / `enforce`）对 HTTP guard 与 Git push 的判定。

本文中 **website** 是角色名（认证面），其实现仓库自 2026-08-21 起为 `gitmono-dev/monoui` 的 `monoengine` 分支 `apps/next-app`（sibling `../monoui`）；Compose 服务名 `website-next`、隔离账户库名 `website` 与 `MEGA_OAUTH__WEBSITE_*` 配置键均未改名。

---

## 认证路径

浏览器身份**只**信任 website Better Auth。monoengine **不签发** session cookie，也**不**直连读取 website 的 user/session 表。与 [`../refactoring/website-auth.md`](../refactoring/website-auth.md) §1 一致：

```text
Browser
  │ Cookie: better-auth.session_token
  │     或  __Secure-better-auth.session_token
  ▼
monoengine HTTP (/api/v1/*)
  │ SessionUser extractor → WebsiteSessionStore::load_user
  │ GET {oauth.website_api_base_url}/api/auth/get-session
  ▼
映射 LoginUser → 注入请求
```

失败一律 HTTP 401（`Login first`），禁止回退到硬编码 admin stub。生产路径禁止旁路 get-session、恢复双后端、或在 monoengine 签发 JWT 冒充会话。

Cookie / CORS 要点（与 website-auth.md §1 一致）：默认按优先级尝试 `better-auth.session_token` 与 `__Secure-better-auth.session_token`；须转发完整 `name=value`；`oauth.allowed_cors_origins` 须含前端 origin 且带 credentials；日志不得打印完整 cookie / token。

双轨认证（与 website-auth.md §3 一致）：

| 通道 | 机制 | 状态 |
|---|---|---|
| `/api/v1` 浏览器 | Cookie → get-session → `SessionUser` | 已落地 |
| API Bearer | Mono `access_token` → `AccessTokenUser` | 保留 |
| Bot | `bot_` Bearer → `BotIdentity` | 保留（见下方 Bot 过渡语义） |
| Git HTTP / LFS | Bearer 或 Basic password = token | 保留 |
| SSH | 公钥 → `ssh_keys.username` | 保留 |

字段映射（website-auth.md §2）：website `name` → monoengine `LoginUser.username`（Cedar principal / Git actor）；`id` → `website_user_id`；`role` **不**进入结构体——管理权以 monoengine ACL / Cedar 为准。

---

## 权限模型

授权事实源是库内 `/.mega_cedar.json` 实体 + `mega_policies.cedar` 策略，经 `[cedar].enforcement` 三态消费（详见 [`../refactoring/contract.md`](../refactoring/contract.md)）。

- **资源归一（ADR-UN-05）：** 单 monorepo 实例下，任意合法请求路径的 Cedar resource **统一归一**到根仓库 `Repository::"/"`；根 ACL 治理全部路径。路径级 ACL 未实现（延后）。
- **组层级：** admin → `matainer` → reader。`matainer` 为历史拼写现状（治理延后 DEFER-UN-06）；策略与 fixture 均按该字面量求值。
- **admin 单源（UN-04）：** enforce 下 admin 能力**唯一**来自实体存储 `UserGroup::"admin"`（由 config `monorepo.admin` 播种）。策略文件中不得再硬编码个人 admin 特例。
- **`pushRepo`：** 仅 maintainer / admin；公开仓库的 reader / 匿名不可 push。
- **merge 面：** merge / merge-no-auth / merge queue 在 enforce 下受 guard 与主体判定约束；改动 `/.mega_cedar.json` 的 CL **仅 admin** 可合（UN-19）。
- **匿名主体：** 匿名 fallback 为保留字 `User::"__anonymous__"`，不得写入 ACL/config 占用。

与 contract.md 同步后的开发事实源以该文件各节为准；本手册只给运维可读摘要。

---

## enforcement 运维

### 三态与开启顺序

配置键：`[cedar].enforcement` = `off` | `shadow` | `enforce`，**默认 `off`**。

| 态 | 行为 |
|---|---|
| `off` | 不构建、不消费授权数据；除已公示的主干删除安全收口外，对外行为与升级前一致 |
| `shadow` | 首建 store、真实评估、记录 would-deny，**不改变放行** |
| `enforce` | 首建 store、真实拒绝（HTTP 403 / 协议错误） |

**强制顺序：** ACL 可信基线审计通过 → `shadow` 启动并完成量化发布门 → 方可 `enforce`。不得跳过 shadow。回退 = 改回 `off` 并重启进程（秒级降级）；主干删除拒绝收口**不**随降级撤销。

### 支持拓扑

- **支持：** `service multi`；纯 `service http` 单进程。
- **不支持：** 独立 `service ssh` 在 `enforcement != off` 时命令层拒启（须迁到 multi，或 ssh 侧保持 `off`）。
- Kill Switch 探测的多个 URL 只允许是**同一进程**的多个入口，且共享同一日志目录。

### enforce 前提清单

生产开启 `enforce` 前须同时满足：

1. 下列执行卡全部已发布：UN-11、UN-02、UN-03、UN-13、UN-16、UN-21、UN-10、UN-18、UN-22、UN-20、UN-23、UN-08、UN-24、UN-25、UN-17、UN-09、UN-04、UN-19、UN-26、UN-27、UN-31、UN-34、UN-30、UN-43、UN-32、UN-38、UN-49、UN-51、UN-54、UN-60、UN-58、UN-57、UN-59、UN-39、UN-35、UN-40、UN-29、UN-37、UN-52、UN-55、UN-56、UN-33、UN-48、UN-41、UN-46、UN-50、UN-36、UN-53、UN-47、UN-42、UN-44、UN-45。
2. ACL 可信基线审计通过（进入 shadow 前一次性；见计划「shadow→enforce 量化发布门」小节）。
3. shadow 量化发布门达标（见下）。

### 快照传播与 ACL 变更保护

- **快照传播（UN-16）：** 主干 `/.mega_cedar.json` 经 merge 漏斗、import 挂接、receive-pack 删除等出入口触发重建；撤权/授权即时生效。receive-pack **拒绝删除** `refs/heads/main`（安全收口，非 enforcement 门控）。
- **重建语义：** 写锁内 **build-then-swap**——先构建完整新快照再整体替换；构建失败则**保留旧快照**并置 **dirty** 标记。`enforce` + dirty = 受护判定**全拒**（fail-closed），直至重建成功清除 dirty。`shadow` 下 dirty 仍记录 would-deny / 告警事件，不改变放行。
- **ACL 变更保护（UN-19）：** 涉及 ACL 文件的合并仅 admin 可合；判定失败在 enforce 下 fail-closed（对外 503 / queue 冻结语义见 contract.md）。

### Bot 过渡语义（UN-27）

Cedar schema 的 principal 仅 `User`。`enforce` 下 Bot 主体在受保护端点求值为 deny。Bot 对受保护 CL 面的操作须在 enforce 前迁移到**用户 token**；完整 Bot 授权模型归后续计划。shadow 期应观察 Bot would-deny。

### 量化发布门（go / no-go）

与计划「里程碑验收与回滚」/「shadow→enforce 量化发布门」一致：

| 项 | 口径 |
|---|---|
| 观察窗口 | `shadow` 在目标部署连续运行 **≥ 7 天**（覆盖至少一个完整工作周） |
| would-deny 分诊频率 | 窗口内全部 would-deny **逐条分诊归零**；未分诊即 no-go；确属误报登记进附录白名单台账后视为已分诊 |
| 告警项 | ①`event=authz_rebuild_failed` ②`event=merge_queue_authz_frozen` ③`event=merge_authz_unavailable`；查询：`rg -n 'event=…' "${MEGA_CACHE_DIR}/logs/"`；任一命中 ≥ 1 即触发 |
| 值班 owner | 通知渠道与具名值班 owner 登记于附录「部署登记表」；未登记不得进入 shadow |
| go 判据 | 窗口达标 + 分诊清零 + 上文 enforce 前提清单 |

shadow 日志取证：结构化字段含 `event=authz_would_deny`（以及上表告警事件名）；禁止把 token/cookie 原文写入证据。

### Kill Switch（紧急改回 off）

运维入口（生产与 fixture 同一文件，v0.2.65 起经 UN-45 原子发布）：

```bash
bash scripts/authz_kill_switch.sh --branch systemd|compose|file --restart -- <重启 argv...>
```

- **变更命令：** 上式；分支由部署登记表选择（`KILL_SWITCH_BRANCH`）。
- **重启范围：** `--restart --` 后的 argv **恰好一次**执行（禁环境变量二次分词）；通常为该进程/unit 的重启命令。
- **回退后验证：** 脚本对 HTTP / 日志 would-deny / Git ls-remote / SSH 做只读探测并落 evidence；**不自称恢复绿**——最终 go/no-go 由执行人依据 evidence 与服务健康独立确认。
- **恢复 go/no-go：** 修复名单或资源后，重开 `shadow` ≥ 3 天且无新增未分诊 would-deny，方可再次 `enforce`。
- **失败终态安全方向：** T1 重启失败 → 退出 4、配置保持 off；T3 rename 后 fsync 失败 → 退出 5、不回滚为 on；T2 探测失败 → 退出 6；跨通道绑定失败 → 退出 7、零写入。

附录「部署登记表」须在进入 shadow 前填齐（未填齐 no-go）。实现细节见 [`../refactoring/integration.md`](../refactoring/integration.md)。

---

## 双系统边界

| 系统 | 职责 |
|---|---|
| website | 登录/注册、session cookie、产品面 `role=admin` |
| monoengine | Cedar ACL、HTTP guard、Git push 授权、admin 组名单 |

website `role=admin` 与 monoengine `UserGroup::"admin"` 是**独立系统**。两边都要授予的用户必须**双写**；自动同步未实现（DEP-02 / DEFER-UN-03）。推荐运维流程：变更 admin 时先更新 config `monorepo.admin` / ACL，再同步 website 管理角色（或反之），并在变更窗口验证两侧；详见 DEP-02 指向与 contract.md「admin 事实源收敛」节。

---

## 身份键现状

现行身份键是 website **展示名** `name` → monoengine `username`（非稳定唯一 ID）。展示名碰撞可能导致 actor / Cedar principal 混同；唯一性由 website 侧运营约束承载（website-auth.md §2）。

身份键向 website 唯一 `username` 的迁移已按 handoff **移交** `plan-long.md` PT-12（DEP-04 outgoing，实际移交日期 **2026-08-17**；ADR-UN-04 Withdrawn；承接强制约束见 DEFER-UN-04）。本计划授权执行继续在现行 `name` 键上工作；GAP-05 关闭以 PT-12 承接计划为准。

---

## 附录：部署登记表

进入 shadow 前逐项填写（参数化环境变量；未填齐 no-go）。字段与计划 Kill Switch runbook 步骤 0 一致：

| 字段 | 说明 |
|---|---|
| `KILL_SWITCH_UNIT` 或 `KILL_SWITCH_COMPOSE` + `KILL_SWITCH_SERVICE` | systemd unit，或 compose 文件与服务名 |
| `KILL_SWITCH_ENV_FILE` | EnvironmentFile 路径（systemd 分支） |
| `KILL_SWITCH_CONFIG` | 实际加载的 config.toml 路径（file 分支） |
| `KILL_SWITCH_BIN` | 已发布 `monoengine` 二进制（fsync / 来源证明） |
| `MEGA_PROFILE` | 有效 profile |
| `KILL_SWITCH_URLS` | 探测 URL 清单（空格分隔；须同进程） |
| `KILL_SWITCH_LOG_DIR` | 日志目录（通常 `MEGA_CACHE_DIR`） |
| `KILL_SWITCH_EXPECT_CODE` | off 下未认证探测端点基线状态码 |
| `KILL_SWITCH_RESTRICTED_DIR` | 受限台账 / evidence 根目录 |
| `KILL_SWITCH_GIT_REMOTE` / `KILL_SWITCH_GIT_ASKPASS` | Git HTTP 只读探测 |
| `KILL_SWITCH_SSH_HOST` / `PORT` / `USER` / `KEY` / `KNOWN_HOSTS` | SSH 只读探测与 pinned host key |
| `KILL_SWITCH_DEPLOY_HTTP` / `GIT` / `SSH` | 三通道 canonical endpoint 绑定 |
| 通知渠道 / 具名值班 owner | shadow 告警与 Kill Switch 升级联系人 |
| ACL baseline 期望 digest 等 | 基线审计独立可信介质字段 |

拓扑边界：多 URL 仅同进程多入口，共享同一 `KILL_SWITCH_LOG_DIR`。

---

## 附录：would-deny 白名单台账

确属误报、经安全负责人审批后登记；有效期 ≤ 90 天，到期复审或移除。

| 条目（principal / action / resource） | 理由 | 审批人 | 登记日期 | 有效期 |
|---|---|---|---|---|

---

## 相关文档

- [`../refactoring/website-auth.md`](../refactoring/website-auth.md) — 认证契约
- [`../refactoring/contract.md`](../refactoring/contract.md) — 授权 / policy / admin / Kill Switch 相关开发契约
- [`../refactoring/integration.md`](../refactoring/integration.md) — Kill Switch 脚本与集成覆盖
- [`../plan/plan-20260812.md`](../plan/plan-20260812.md) — 本能力日期计划（含量化发布门与 runbook 权威细则）
- [`monorepo-init.md`](./monorepo-init.md) — monorepo 初始化（含 `admin` 播种）
