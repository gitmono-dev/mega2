# Monoengine integration tests

This document describes the active integration-test contract. Compose service
registration and lifecycle rules live in [`test-infra.md`](./test-infra.md).

## Test stack

`docker-compose.test.yml` provides PostgreSQL, Redis, RustFS, optional
profiled `git-cli`, profile `app` monoengine, and profile `web` website-next.
Use the fixed project name:

```bash
docker compose -p monoengine-it -f docker-compose.test.yml up -d --wait
```

The embedded `VaultCore` is part of monoengine; no external Vault container is
used. Database schemas must be created through the project's migrations.

Mailpit remains an optional capture service for website authentication and
website product-email IT only. Monoengine never connects to it, and neither
the default integration suite nor CI requires a monoengine SMTP success path.

## Active integration targets

Git 用户场景的完整矩阵（HTTP/SSH/auth/repo-shape、字面 `git pull`、LFS、DEFER）以
[`protocol.md` 的「场景覆盖表」](./protocol.md#场景覆盖表权威plan-20260803--adr-gm-01)
为唯一事实源；本文件只登记 cargo target 索引，不复制矩阵单元格。

| Target | Purpose | Prerequisites |
| --- | --- | --- |
| `integration_vault` | CLI config, Vault bootstrap, redaction, and HTTP smoke | PostgreSQL and Redis |
| `integration_git_cli` | Git HTTP protocol round trips | PostgreSQL, Redis, `--profile git` |
| `integration_git_lfs` | Git HTTP LFS push → fetch → `git lfs pull` round trip（plan-20260803 / GM-05） | PostgreSQL, Redis, `--profile git`, git-lfs |
| `integration_git_ssh` | Git SSH cargo-native self-start clone/pull/push（plan-20260803 / GM-06..08） | PostgreSQL, Redis, `--profile git` |
| `integration_website_auth` | Better Auth cookie to monoengine session bridge | `--profile app --profile web`, `WEBSITE_IT=1` |
| `integration_website_mail` | Website internal product-email API acceptance (Bearer + allowlisted event → 202; bad bearer → 401) | `--profile app --profile web`, `WEBSITE_IT=1`, website tip with internal mail route |
| `integration_authz_audit` | 只读装配零副作用黑盒（UN-30 / UN-43）+ `authz-audit` CLI（UN-29 审计/fsync + UN-37 promote） | PostgreSQL, Redis，且必须 `-- --test-threads=1` |

Run the normal project gate with the test environment loaded:

```bash
source .env.test && cargo test --all
```

For the real website-session and internal-mail checks:

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app --profile web up -d --wait
source .env.test
WEBSITE_IT=1 cargo test -p monoengine --test integration_website_auth -- --test-threads=1
WEBSITE_IT=1 cargo test -p monoengine --test integration_website_mail -- --test-threads=1
```

When `WEBSITE_IT=1` is set, unavailable monoengine or website-next endpoints
are failures, not passing skips.

## Notification and product-email coverage

Monoengine tests cover trigger selection, user preferences, in-app delivery,
optional Slack/webhook handling, and the website-mail client’s request/error
behavior. The website owns product-email rendering and delivery.

Do not add or retain tests that:

- seed or query `email_jobs`;
- set `[mail]`, `mail.password`, or SMTP endpoint configuration;
- require `SmtpMailer` to deliver to Mailpit;
- treat Mailpit availability as a monoengine startup gate.

Website email capture, if needed, belongs to website-next’s test provider
configuration and may use the Compose `mailpit` service. See
[`website-mail.md`](./website-mail.md).

## 授权首建与 shadow→enforce 集成（UN-02）

HTTP 服务启动时在 listener 绑定前完成共享授权快照首建（`ensure_authz_first_build`，ADR-UN-02）：`off` 不构建；`shadow`/`enforce` 下首建失败使 server 启动失败。push 门三态（`check_push_permission`）在 `shadow` 下放行并记录 would-deny，`enforce` 下拒绝无权限 push。SSH 面（UN-03）与 HTTP 面共享同一 `AppContext.entity_store` 实例；独立 `service ssh` 对 `enforcement != off` 拒绝启动（指引 `service multi` 或 `off`）。UN-02 的 5 门 shadow→enforce 判据脚本使用 `MEGA_IT_PG*` 环境变量（登记于 `.env.test.example`，ER-11：凭据只经环境变量）。

## 授权快照传播与主干删除防护集成（UN-16）

`integration_git_cli` 与 `integration_git_ssh` 覆盖快照传播闭合的端到端行为：

- `integration_git_cli_authz_revoke_grant_immediate_effect`：`enforce` 下非 admin push 被拒 → admin 合并授予变更后同一 push 通过 → 合并撤销变更后再次被拒，全程不重启服务。
- `integration_git_cli_rejects_main_branch_delete`：receive-pack 删除主干 ref 被拒绝，错误对 git 客户端可操作。
- `integration_git_ssh_authz_grant_immediate_effect`：ACL 变更经 HTTP 腿的 merge 漏斗合入，SSH 腿在下一次 push 即刻生效——`service multi` 下两腿共享同一实例的直接证据。

授权变更后再次 push 前必须先 `git fetch` 并从新的 `origin/main` 起分支：`MonoRepo::check_entry` 只接受每次 push 携带单个 commit，陈旧克隆会把父提交一并打包而被拒。

写点 allowlist 守卫（`scripts/authz_write_points_guard.sh`）与其两个自测变异（`--selftest-add` / `--selftest-remove`）是三道独立门，任何未登记的主干 ref 写入口都会使守卫非零退出。

## Kill Switch 骨架与安全写原语（UN-33；REL-02）

运维入口：`bash scripts/authz_kill_switch.sh`（生产与 fixture 同一文件；家族内后续卡在同一 `--selftest` 入口叠加门）。

- **分派**：`--branch systemd|compose|file`（目标分别取自 `KILL_SWITCH_ENV_FILE` / `KILL_SWITCH_COMPOSE` / `KILL_SWITCH_CONFIG`）。无 `--apply-content` 时走 UN-41 变换；有则仅为骨架/元数据低层替换。可选 `--restart -- <argv...>`（UN-46：argv 数组恰好一次执行，禁二次分词）。启动时先跑 UN-50 preflight；重启成功后若设置 `KILL_SWITCH_URLS` 则跑 UN-36 HTTP 探测。
- **preflight（UN-50）**：Linux 专属；校验 cp/yq/jq/rg/flock/stat/getfacl/getfattr 存在性、GNU `cp --preserve=all`、`KILL_SWITCH_BIN authz-audit fsync --probe`、版本下限（coreutils≥8.30 / yq≥4.18 / jq≥1.6 / rg≥13 / util-linux≥2.27）；任一缺失/不足 fail-closed 并给出安装指引。
- **HTTP 探测（UN-36）**：`KILL_SWITCH_URLS` + `KILL_SWITCH_EXPECT_CODE` + `KILL_SWITCH_DEPLOY_HTTP`；canonical origin 绑定（默认端口/IPv6）；https 强制证书链校验；失败终态 T2 → 退出码 6、配置保持 off；**不自称恢复绿**。
- **evidence 记录（UN-53）**：探测前经 `KILL_SWITCH_BIN authz-audit run-init --restricted-root "$KILL_SWITCH_RESTRICTED_DIR"` 分配 run；脚本自持 `runs/<run-id>/.lease.lock`（长生命周期，≠ UN-56 `.evidence.lock`）；逐项经 `evidence-append`（channel/check/verdict[/status]）写入，禁止脚本内拼 JSON；成功 `run-commit`（`RUN_CAP`），异常 `run-abort`；256 KiB 硬上限与未知枚举拒绝由 CLI 透传。
- **日志通道（UN-47）**：`KILL_SWITCH_LOG_DIR`；重启前按 inode 捕获字节游标；重启后有界轮询落盘（默认 30×2s）；只读增量；`rg would-deny` 按 0/1/>1 分支（>1 硬失败、不转零命中）；零新增则 `log_no_would_deny=pass`，否则 T2。
- **Git HTTP 只读（UN-42）**：`KILL_SWITCH_GIT_REMOTE` + `KILL_SWITCH_DEPLOY_GIT` + `KILL_SWITCH_GIT_ASKPASS`；`git ls-remote`（凭据仅经 ASKPASS，禁 argv）；canonical origin 绑定；stderr 脱敏；可选 `KILL_SWITCH_GIT_ZEROCHECK_REPO` 断言 refs/objects 零变化。
- **SSH 只读与跨通道绑定（UN-44）**：`KILL_SWITCH_SSH_HOST/PORT/USER/KEY` + `KNOWN_HOSTS` + `REPO` + `DEPLOY_SSH`；启动时三通道 early bind（不符 → 退出码 7、零写入）；pinned known_hosts；`git ls-remote` over SSH。
- **变换与读回（UN-41）**：systemd 将 `MEGA_CEDAR__ENFORCEMENT=` 替换为 `=off`；compose 仅 mapping 形态经 `yq` 写 `"off"`（list/缺键 fail-closed）；file 改写 `[cedar].enforcement` 为 `"off"`（缺键 fail-closed），必选 `MEGA_PROFILE`，并以 UN-01 `config validate --show-sources --format json` 断言 winning source 为该文件（环境源获胜则禁用）。落盘后恰好一次读回校验为 `off`（`KILL_SWITCH_READBACK_FAIL=1` 可注入）。
- **写原语**：同目录临时文件 `O_EXCL|O_NOFOLLOW` → `cp --preserve=all` 播种 → 原地改写字节 → 元数据逐项校验 → `KILL_SWITCH_BIN authz-audit fsync <tmp>` → `renameat` → `authz-audit fsync <target>`；目标须为 no-follow 普通文件；systemd EnvironmentFile 拒绝重复键。
- **重启与失败终态（UN-46）**：`--restart -- <argv...>` 恰好一次；T1 重启失败 → 退出码 4、配置保持 off、打印人工重启指引；T3 rename 后 fsync 失败 → 退出码 5、不回滚为 on、重跑幂等收敛。
- **元数据保持（UN-48）**：owner / mode / ACL / xattr；写后 `stat`/`getfacl`/xattr 比对，不符即失败（可用 `KILL_SWITCH_META_VERIFY_FAIL` 注入）。
- **自测**：`bash scripts/authz_kill_switch.sh --selftest`（… + UN-42×6 + UN-44×7 = **58/58**；fsync stub = `scripts/authz_kill_switch_fsync_stub.sh`）。需可用的 `KILL_SWITCH_BIN` 或 `target/debug/monoengine`；compose/preflight 需 `yq`（或 `KILL_SWITCH_YQ`）；缺 `getfattr` 时自测会在临时 PATH 放入存在性 stub。
- **发布**：REL-02 十子卡本地提交后由 UN-45 原子发布为 **v0.2.65**（脚本入口 / 三分支 / 拓扑绑定 / 失败终态安全方向见上；降级 = 不执行该脚本）。

## 迁移覆盖：`mega_cl.link` 唯一索引（UN-10）

`m20260815_000000_unique_mega_cl_link` 为 `mega_cl.link` 建唯一索引 `idx_mega_cl_link_unique`（既有索引只有复合的 `(path, link)`，`link` 单列查询既非索引等值探测也不保证唯一）。`up` 在同一事务内先取 `LOCK TABLE mega_cl IN SHARE ROW EXCLUSIVE MODE`，再扫描重复 `link`；发现重复即中止并在错误里列出全部冲突 link 与各自行数，不修改任何业务行。锁从扫描前持有到索引建成，杜绝「扫描通过后、建索引前插入重复」的竞态。

恢复为 forward-only：运行时 runner 只暴露 `up`/`refresh`，已发布索引只能由新迁移前滚修复；`down` 仅供隔离测试与开发 `refresh`，不得作为生产回退。

迁移模块内 `#[cfg(test)]` 覆盖四条：`up` 建唯一索引且 `down` 删除、重复数据使 `up` 带完整冲突清单失败且行数不变、并发插入在迁移事务提交前被锁阻塞（提交后被唯一索引拒绝）、`SET LOCAL enable_seqscan = off` 下 `EXPLAIN` 的计划命中新索引名。`get_cl` 的按 link 回归在 `src/jupiter/storage/cl_storage.rs` 内。

## 迁移覆盖：`merge_queue.requester` nullable 列（UN-18）

`m20260815_000100_merge_queue_requester` 为 `merge_queue` 增加 nullable `requester` 列（无默认值、无回填、不修改既有行）。排队的合并由后台 worker 稍后执行，请求主体必须随队列项持久化，否则执行时没有自己的授权主体。本卡只加列：写入/读出链归 UN-20，NULL（legacy 行）的执行判定归 UN-17。

**恢复路径按消费面状态区分（forward-only，运行时无 down 入口）：**

- **pre-consumer**（UN-20 未发布，列恒为空）：可由新迁移前滚删列。
- **post-consumer**（列已在写入 requester）：**禁止无条件删列**——会丢失授权主体与审计数据。只能保留列并停用消费方，或以新列替代并复制数据。

迁移模块内 `#[cfg(test)]` 覆盖：列为 nullable 且无默认值、既有行读回 NULL、隔离库内 `down` 删列后 `up` 可前滚回已发布状态。

> 写迁移测试时注意：单元测试库按 **schema** 隔离（`search_path`，见 `src/jupiter/tests.rs`），而 `information_schema` / `pg_indexes` 跨全部 schema，因此目录查询必须带 `current_schema()` 限定，否则会读到并发用例的表。

## merge queue requester 读写链（UN-20）

`/merge-queue/add` 与 `/merge-queue/retry/{cl_link}` 经 `OptionalSessionUser`（UN-22）捕获**可选**主体：该 extractor 永不 401，因此匿名入队/重试仍可服务，记为 NULL——本卡不产生任何新的拒绝面。

链路：router → `MonoApiService::{add_to_merge_queue_as, retry_merge_queue_item_as}` → `MergeQueueService::{add_to_queue_with_requester, retry_queue_item_with_requester}` → storage。requester 与队列行**在同一条插入/更新语句**里落库——排队的合并稍后由后台 worker 执行，行存在而主体缺失就是一次没有主体的合并。

既有 storage 方法签名零变更：`add_to_queue` / `retry_failed_item` 保留并委派。语义差别值得记住：`retry_failed_item`（不知道主体）**不动**已记录的 requester，而 `retry_failed_item_with_requester(link, None)`（显式匿名）会把它清空。

读出面 `get_requester(cl_link) -> Option<Option<String>>`：外层 `None` = 没有该队列行，内层 `None` = 匿名或 UN-18 之前写入的 legacy NULL 行。执行时的主体判定由 UN-17 消费该读出面。

> 单测提示：`jupiter::tests::test_storage` 的 `merge_queue_service` 原本是 `mock()`（disconnected 连接），驱动队列链路会 panic；已改为与该 storage 共用同一测试库。

## CI

`.github/workflows/config-validation.yml` runs formatting, Clippy, the
compose-backed integration targets (including `integration_website_auth` and
`integration_website_mail` under `WEBSITE_IT=1`), and the real website session
check after checking out the `orbit` and `monoui` siblings. Product email is
proven via the website internal API + `EMAIL_PROVIDER=test`; no local SMTP
dependency is required for monoengine notification paths.

## 只读装配的零副作用比对（UN-30 / UN-43）

`integration_authz_audit` 的立论方式是**前后对照**，不是通读代码。播种走**真实二进制**（`monoengine service http` 起来再 SIGINT），因此迁移、`init_monorepo()` 写下的 refs 与对象、默认 sidebar 全都真的发生过——手搓出来的库只包含我们想到的东西，而「零写入」恰恰是关于没想到的那些。

快照有七个面：schema（范围是除系统 schema 外的**全部** schema）、每表**内容摘要**（整行转文本排序聚合后 md5，而不是行数——一次 UPDATE 不改变计数）、`pg_sequences` 当前值（被回滚的插入不留行却推进序列，那同样是一次写）、`mega_refs` 全行、三张对象表全行、对象存储目录逐文件散列，以及 `MEGA_BASE_DIR` + `MEGA_CACHE_DIR` 的逐文件指纹（vault 的 `core_key.json` 就在这一面里）。文件指纹是**内容散列 + 尺寸 + 权限位 + mtime**：一次「内容相同」的重写不会改变内容散列，却仍然是一次写，只有 mtime 能发现它。目录本身也收（路径 + 权限位），因此创建/删除/改名目录与只改目录权限都看得见。

用例分两幕：

1. **本地对象存储**：只读装配读根 ref 与授权源（这条路径同时用到 DB 与对象存储），前后七面全等。
2. **需要 vault 的部署**（UN-43）：先用 `config secret set` 把两个对象存储凭据写进 vault，再切到 s3compatible，于是只读装配**真的**打开 vault。断言它拿到的是 UN-31 的只读句柄、`denied_writes()` 为 0，且 vault 表与 core key 文件一字未动——bootstrap 路径会轮换 runtime 凭据并回写 key 文件，那正是这一幕要排除的东西。用本地存储时 vault 根本不会被打开，「Vault 状态零变化」会退化成一句空话，所以第二幕不能省。

最后做两次**量具校准**：插入一行探针（快照必须变）、再原地改写同一行（行数不变、摘要必须变）。否则「前后相等」可能只是因为这份快照什么都没量到。

受限根目录产物的排除项写在 `RESTRICTED_ROOTS` 常量里，今天为空；留着是因为「排除了什么」必须是一份明写的清单，而不是某处 diff 里悄悄少掉的几行。匹配用**路径前缀**而不是子串——受限根目录是一个路径祖先，子串匹配既会误伤中间路径同名的文件，也会漏掉本该排除的；且比的是**相对于受监视根**的路径（受限根目录就是这样登记的），拿拼好的绝对路径去比前缀永远对不上，排除清单会变成一个从不生效的摆设。

**这个 target 只有一个用例，且必须 `--test-threads=1`**：它用 `std::env::set_var` 把数据库与对象存储指向本用例专属的库（`.env.test` 里的 `MEGA_DATABASE__DB_URL` 指向共享 admin 库，而环境变量优先级高于配置文件）。这条约束不是靠注释请求后来者小心，而是**运行期强制**：第二次构造 fixture 直接 panic 并说明原因——两个 fixture 会互相覆盖对方的数据库地址，而且谁都不会报错，于是比对会静悄悄地跑在别人的库上。

**这一面量得到什么、量不到什么**：快照收录的是「最终存在的条目」，因此**创建后又删除**的临时文件不会被发现；两个受监视根目录之外的写也不会。前者是快照法的固有边界（要抓它需要的是事件流而不是快照），后者是登记范围的选择。两条都写在这里，是因为一份不说明边界的「零副作用」证明会被读成比它实际更强的东西。
