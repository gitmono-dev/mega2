# 配置参考

[English](configuration.md) · 中文

本文是 mega2 配置系统的导览与运维参考：加载顺序、密钥管理、热加载、分区说明、启动校验与 `config validate`。逐键的带注释完整样例以 [`config/config.toml`](../config/config.toml) 为权威源，本文不复制键值表；各子系统的契约细节另见 [`refactoring/config.md`](./refactoring/config.md)（object_format / cedar / push_auth / storage_events 等冻结语义）。产品规则见 [`monorepo.md`](./monorepo.md)，trunk 部署运维见 [`deploy-trunk.md`](./deploy-trunk.md)。

配置模块在 `src/config/`（loader / model / source / expand / secret / validate / reload）。全局 flag 为 `--config <path>` 与 `--profile <name>`（等价环境变量 `MEGA_CONFIG` / `MEGA_PROFILE`）。

## 1. 加载顺序与来源

基础配置文件按以下顺序取**第一个命中**（`src/config/loader.rs`）：

1. `--config <path>`（CLI，来源名 `cli`）
2. `MEGA_CONFIG` 环境变量（`env`）
3. `./config/config.toml`（当前工作目录，存在才命中，`cwd`）
4. `$MEGA_BASE_DIR/etc/config.toml`（`global`）
5. 以上全无 → 生成默认配置写入 `$MEGA_BASE_DIR/etc/config.toml`（`default_generated`）

只读 / 需既有配置的命令（如 `config validate`、`service init`）走 `load_readonly` / `load_existing`：第 1、2 条指名的文件不存在则直接报错，且**不会**生成默认配置——不会在它要观察的系统上留下副作用。

**Profile**：`--profile prod`（或 `MEGA_PROFILE=prod`）选取基础配置旁边的兄弟文件 `config.prod.toml`（由基础文件名派生：`<stem>.<profile>.<ext>`）。profile 名只允许 ASCII 字母 / 数字 / `-` / `_`；文件不存在即报错。CLI 的 `--profile` 优先于 `MEGA_PROFILE`。合并顺序：**基础文件 → profile 文件 → 环境变量**（profile 在 env 之前合并，env 永远最后胜出）。

**环境变量覆盖**：模式为 `MEGA_<SECTION>__<KEY>`，双下划线是层级分隔符（`src/config/source.rs`）。示例：

```bash
MEGA_DATABASE__DB_URL='postgres://mono-pg:5432/mono' \
MEGA_OAUTH__ALLOWED_CORS_ORIGINS='https://app.example.com,https://app2.example.com' \
  mega2 --config /etc/mega2/config.toml service http
```

**严格模式**：未知字段与已移除字段在解析期即被拒绝（`reject_unknown_fields`），不会静默忽略——例如旧 `[mail]` 段、`[monorepo].merge_writer` 残留都会拒绝启动。`config validate` 以 warning 报告被忽略的兼容性字段（不打印密值），自动化中可用 `--deny-warnings` 将其升级为失败。

## 2. 密钥管理

凭据不写进提交的 `config.toml`。两种注入方式：

- **SecretRef**：`vault://secret/<name>#<field>`，由内嵌 Vault（`libvault` crate + `src/contract/vault/`，契约见 [`refactoring/vault.md`](./refactoring/vault.md)）解析。`SecretRef` 的 `Debug` / `Display` 恒为脱敏的 `vault://secret/***#***`，错误文本不含真实路径与值。
- **文件挂载**：`${file:/run/secrets/xxx}` 在加载期展开为文件内容（`src/config/expand.rs`）。含 `${file:...}` 的值只能由占位符与字面文本组成，不得与 `${var}` 混用；未闭合的 `${file:` 同样被拒绝。

可用 mega2 Vault 托管的配置密钥字段（`config secret` 只接受这些，见 `src/commands/config.rs`）：`redis.url`、`notification.webhook.token`、`object_storage.s3.access_key_id`、`object_storage.s3.secret_access_key`、`storage_events.targets.<id>.secret_ref`。**数据库凭据被刻意排除**——它是 Vault 自身的引导依赖，必须留在部署 / 环境密钥里。每个字段有固定命名空间 `config/<profile>/<suffix>`（如 `config/prod/redis/url`），`config validate` 只校验形状不解析。

```bash
# 生成规范 URI（不接触 Vault）
mega2 --config /etc/mega2/config.toml config secret ref \
  --vault-path config/prod/redis/url --field value

# 写入 / 轮换（值只从 stdin 读，--value-stdin 必需）
printf '%s' "$REDIS_URL" | mega2 --config /etc/mega2/config.toml \
  config secret set redis.url \
  --vault-path config/prod/redis/url --field value --value-stdin
mega2 --config /etc/mega2/config.toml config secret rotate redis.url \
  --vault-path config/prod/redis/url --field value --value-stdin < /run/secrets/new-redis-url

# 校验可解析（不打印值）
mega2 --config /etc/mega2/config.toml config secret check redis.url \
  --vault-path config/prod/redis/url --field value
```

`rotate` 之后需重启消费该密钥的服务；新发生的解析与 `config validate --resolve-secrets` 立即用新值。

**Vault 运维**（`config vault *`，均走 VaultBootstrap 加载，不启动服务）：`reset`（重建，破坏性，需 `--force`，旧 core key 自动备份）、`rekey`（重切 unseal shares，需 `--force`；注意只重切当前 KEK 的份额，旧份额集仍可解封，要使旧份额失效需完整 KEK rotation）、`backup <destination>`（导出 core key + `.meta.json`）、`restore <source>`（覆盖 core_key.json，需 `--force`，恢复后立即验证可解锁）。`--key-path` 缺省为标准 vault 数据目录下的 `core_key.json`。

**[vault.audit]**：密钥读写审计，默认开启。`sink = "tracing"`（默认，`vault_audit` target，不可失败）或 `"file"`（追加式 JSONL、逐条 fsync，需配 `file_path`）；`fail_closed = true` 时审计落盘失败会使密钥操作本身失败（默认 false，fail-open）。审计记录只含操作、逻辑名、结果与调用方，**永不含密值**。

## 3. 热加载

服务运行期间 `ConfigReloadWatcher` 以 **5 秒轮询**（`src/commands/service/mod.rs` 的 `CONFIG_RELOAD_POLL_INTERVAL`）监视基础配置与 profile 文件的 mtime/长度变化。候选配置先过完整 `Config::validate`，非法即拒绝并保留当前快照；合法则按字段分流（`src/config/reload.rs`）：

**可热生效**（`applied_fields`，订阅者 apply 失败会整体回滚、不发布新快照）：

- `log.level` / `log.print_std` / `log.with_ansi`
- `artifacts_gc.interval_secs` / `grace_secs` / `batch_limit`；`artifacts_gc.enable` 仅 **true → false** 热生效，false → true 需重启
- `buck.cleanup_interval` / `completed_retention_days`；`buck.enable_session_cleanup` 仅 **true → false** 热生效，从 false 开启需重启
- `notification.enabled`（notification 段快照整体热替换；enabled 翻转计入 applied）

**其余全部字段**（`database.*`、`redis.url`、`base_dir`、`monorepo.*`、`git.*`、`pack.*`、`lfs.*`、`blame.*`、`object_storage.*`、`oauth.*`、`storage_events.*`、`github_sync.*`、`buck` 上传限额、`vault.audit`、`cedar` 等）变更只记入 `restart_required_fields`，日志可见，快照不更新——需重启进程生效。热加载不会把新密值写回快照，报告里也不出现密值。

## 4. 分区指南

每节给出一行用途与关键条目；完整注释与默认值以 [`config/config.toml`](../config/config.toml) 为准。除第 3 节列出的白名单外均为 restart-required。

- **`base_dir`**（顶层）：数据根目录（日志、本地对象、LFS、缓存），可用 `MEGA_BASE_DIR` 覆盖；样例中 `${base_dir}` 占位符在各路径字段展开。
- **`[log]`**：tracing 日志。`level`（trace..error）、`print_std`（生产关）、`with_ansi`（仅 stdout）。全部热加载。
- **`[database]`**：仅支持 PostgreSQL（`db_type = "postgres"`）。`db_url`、连接池 `max_connection` / `min_connection`、`acquire_timeout` / `connect_timeout`、`sqlx_logging`。凭据用 `MEGA_DATABASE__DB_URL` 注入，不走 Vault SecretRef。
- **`[monorepo]`**：产品规则（权威 [`monorepo.md`](./monorepo.md)）。`import_dir`（默认 `/third-party`，ImportRepo 多分支特例）、`admin`、`root_dirs`（目录初始化）、`object_format`（`sha1` 默认；`sha256` / `blake3` 为 Libra 扩展，见 [`refactoring/config.md`](./refactoring/config.md) 与 [`refactoring/protocol.md`](./refactoring/protocol.md)）、`push_policy`（`review` 默认 / `trunk`）、`max_push_commits`（trunk 链长上界）。bootstrap：`mega2 --config <path> service init --yes`（见 [`manual/monorepo-init.md`](./manual/monorepo-init.md)）。
  - 路径形状（`config validate`、服务启动与热重载候选同一校验，违反即失败并点名字段）：`root_dirs` 每项为唯一的单组件目录名——不含 `/`、`\` 或 NUL，不能是 `.`、`..` 或空串，无首尾空格，且不能是根树保留项（`.cedar`、`.mega_cedar.json`、`.buckroot`、`.buckconfig`、`.git`）；`import_dir` 为规范绝对非根路径（无尾斜杠，无 `//`、`.`、`..` 段，不含 NUL 与 `\`），且首组件必须列在 `root_dirs` 中。**import 优先**：`import_dir` 之下的路径一律按 ImportRepo 处理（由 Git 推送创建），即使 `root_dirs` 含同名项。`root_dirs` 只在首次初始化（`service init` 或空库上的首次服务启动）时建目录；事后修改需重启，且不会增删已初始化库的顶层目录（见 [`manual/monorepo-init.md`](./manual/monorepo-init.md)）。
- **`[monorepo.rename]`**：diff 分类的移动 / 改名检测：`similarity_threshold`（0-100）、`rename_limit`（0 = 不限制）。
- **`[pack]`**：receive-pack 解码资源。`pack_decode_mem_size` / `pack_decode_disk_size`（支持 K/M/G、KiB/MiB 与百分比）、`pack_decode_cache_path`、`clean_cache_after_decode`、`channel_message_size`、`save_entry_concurrency`。
- **`[lfs]`**：`[lfs.ssh].http_url`（SSH 传输的 href 底座，LFS 文件仍走 HTTP）、`[lfs.local].lfs_file_path`。trunk 下 LFS 鉴权随 `git.push_auth`，见 [`deploy-trunk.md`](./deploy-trunk.md) §6。
- **`[object_storage]`**：Git blob / LFS / artifact 的全局后端，`storage_type = "local" | "s3" | "s3compatible" | "gcs"`，子段 `[object_storage.s3]`（region / bucket / access_key_id / secret_access_key / endpoint_url）、`[object_storage.gcs]`、`[object_storage.local]`。后端经 `build_object_storage` 构建，契约见 [`refactoring/orbit.md`](./refactoring/orbit.md)。`config validate` 拒绝 access_key_id / secret_access_key 里的 `vault://` 值——bootstrap 凭据用 env / profile / 部署密钥注入。
- **`[oauth]`**：website Better Auth 会话校验。`website_api_base_url`、`session_cookie_names`（省略走 Better Auth 默认 cookie 名）、`allowed_cors_origins`（空则用内置开发默认值；env 覆盖为逗号分隔列表）。信任边界见 [`refactoring/website-auth.md`](./refactoring/website-auth.md)。
- **`[blame]`**：大文件阈值与遍历资源：`max_lines_threshold`、`max_size_threshold`、`default_chunk_size`、`max_commits_in_memory`、`enable_caching`。
- **`[redis]`**：`url`。缓存 / 分布式锁 / snowflake worker 租约等；可作 Vault SecretRef 托管。
- **`[buck]`**：Buck 上传 API：会话与文件限额（`session_timeout` / `max_file_size` / `max_files` / `max_concurrent_uploads`）、服务端并发限流（`upload_concurrency_limit` / `large_file_concurrency_limit` / `large_file_threshold`）、会话清理任务（`enable_session_cleanup` / `cleanup_interval` / `completed_retention_days`，部分热加载，见第 3 节）。
- **`[artifacts_gc]`**：无引用 artifact blob 的 GC（`enable` / `interval_secs` / `grace_secs` / `batch_limit`），默认关；运行中调参热加载，从关闭到开启需重启。
- **`[notification]`**：`enabled` 全局开关（热加载）；可选 `[notification.webhook]` 出站通道（`url` 非密，`token_ref` 为 SecretRef）。行为与边界见 [`refactoring/notification.md`](./refactoring/notification.md)。
- **`[vault.audit]`**：见第 2 节。
- **`[cedar]`**：`enforcement = "off" | "shadow" | "enforce"`（ADR-UN-01；默认 off）。trunk 形态必须为 `off`。语义与快照构建见 [`manual/authz.md`](./manual/authz.md)。
- **`[git]`**：Git 协议与产品 API 写的认证。`push_auth`（省略 = OAuth/UserStorage 链，仅 review；`"token"` / `"none"` 要求 trunk）、`[[git.push_tokens]]`（name / token / paths，组件边界前缀授权，token 用 `${file:...}` 或 SecretRef）、`ssh_receive_pack`（storage-only 必须显式 `false`）、`anonymous_access`。完整语义见 [`deploy-trunk.md`](./deploy-trunk.md) §§2–4。
- **`[oci]`**：`enabled` 开关；`/v2` 双重门（storage-only + enabled），缺一只返回裸 404。见 [`refactoring/oci.md`](./refactoring/oci.md) 与 [`deploy-trunk.md`](./deploy-trunk.md) §10。
- **`[agent_capture]`**：`enabled` + `tenant_id` / `deployment_id` + `[[agent_capture.ingest_tokens]]`（name / token / paths）。storage-only only，开启需显式 `git.push_auth` 与至少一个 token。见 [`refactoring/agent-capture.md`](./refactoring/agent-capture.md)。
- **`[storage_events]`**：已提交写入的出站事件（默认关）。`enabled`（要求 storage-only）、`installation_id`（必填、不自动生成）、并发 / 超时 / 优雅关闭数值界、`[[storage_events.targets]]`（id / https url / `secret_ref` HMAC / events / 过滤器列表）。冻结校验规则见 [`refactoring/config.md`](./refactoring/config.md) 与 [`refactoring/storage-events.md`](./refactoring/storage-events.md)。
- **`[github_sync]`**：GitHub 出站同步（默认关）。`enabled`、`ssh_host` / `ssh_user` / `ssh_host_key`、`ssh_key_ref`（SecretRef 字符串，勿写私钥本体）、四个超时、`[[github_sync.bindings]]`（id / path / remote）。见 [`refactoring/github-sync.md`](./refactoring/github-sync.md)。

## 5. 启动期 fail-closed 校验

`Config::validate` / `AppContext::new` 在启动期拒绝不合规配置，而不是带病运行。trunk / storage-only 形态的不变式清单（`cedar.enforcement` 必须为 off、无 open CL、`push_queue` 非终态行、`push_auth` 显式且与形态匹配、`ssh_receive_pack = false` 等 7 条）以 [`deploy-trunk.md`](./deploy-trunk.md) §1 为权威，本文不复述。其余代表性拒绝项：未知 / 已移除字段、`cedar.enforcement` 非法值、`monorepo.admin` 含保留匿名主体、`[oci]` / `[agent_capture]` / `[storage_events]` 在非 storage-only 下开启、`storage_events` 数值越界与非规范过滤器、SecretRef 命名空间不符。错误文本统一走 [`errors.md`](./errors.md) 的约定且不含密值。

## 6. `config validate`

改完配置先验证再启动服务：

```bash
mega2 --config /etc/mega2/config.toml config validate
# 含 profile 与来源诊断
mega2 --config /etc/mega2/config.toml --profile prod \
  config validate --show-sources
# CI：warning 即失败；密钥引用实际解析（不打印值）
mega2 --config /etc/mega2/config.toml \
  config validate --deny-warnings --resolve-secrets
```

- `--resolve-secrets`：引导 Vault 并解析配置中的 SecretRef，验证可达性与字段存在性；解析结果不打印、不写回。
- `--deny-warnings`：来源诊断（被忽略字段、env 映射问题等）存在 warning 时以非零退出。
- `--show-sources`：打印 base / profile / env 三层的字段来源与覆盖关系；`--format json` 输出冻结的 JSON 模式（含 `cedar.enforcement` 的 winning source，ADR-UN-01）。
- `config init [--output <path>] [--force]`：写出带注释的模板配置，打印后续 `config secret set` 与 `config validate --resolve-secrets` 指引。

`config validate` 走只读加载（第 1 节）：不生成默认配置，指名文件缺失即报错。开发与测试环境的配置约定见 [`development.md`](./development.md)；仓库级约定见 [`AGENTS.md`](../AGENTS.md)。

## 7. 相关文档

- 本套文档：[`quick-start.zh.md`](./quick-start.zh.md) · [`user-guide.zh.md`](./user-guide.zh.md) · [`deployment.zh.md`](./deployment.zh.md) · [`architecture.zh.md`](./architecture.zh.md) · [`contributing.zh.md`](./contributing.zh.md)
