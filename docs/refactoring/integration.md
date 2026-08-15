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

## CI

`.github/workflows/config-validation.yml` runs formatting, Clippy, the
compose-backed integration targets (including `integration_website_auth` and
`integration_website_mail` under `WEBSITE_IT=1`), and the real website session
check after checking out the `orbit` and `website` siblings. Product email is
proven via the website internal API + `EMAIL_PROVIDER=test`; no local SMTP
dependency is required for monoengine notification paths.
