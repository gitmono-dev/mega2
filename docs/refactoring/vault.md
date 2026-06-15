# Vault 模块现状与改进计划

本文档记录 `monoengine` 当前 `vault` 模块的实现形态、主要风险、与配置系统的依赖关系，以及后续分阶段改进计划。本文与 `config.md` 中关于敏感配置、`SecretRef`、最小 DB/Vault bootstrap 和 `core_key.json` 加固的约束保持一致。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **集成测试指引**：本计划的各阶段应通过 **`integration.md`** 中定义的集成测试进行端到端验证，特别是 Vault 初始化、Secret 存储与轮换、fail-closed 行为和最小 bootstrap 能力应在 Docker 环境中完整测试。

> **仓库格式说明（2026-06-16）**：当前工作副本由 **Libra** 管理，不是传统 `.git` 工作树。执行、评审或核对本计划时，应使用 `libra status`、`libra diff -- <path>`、`libra add` 等 Libra 命令检查工作区状态与差异；不要把 `git status` / `git diff` 失败误判为“不是仓库”。本文中“Git 协议 / Git 托管 / 不进入 git”描述的是 monoengine 的业务域和兼容目标，不代表当前开发工作区必须由 Git 管理。

> **依赖迁移修订（2026-06-15）**：`libvault-core`（crates.io `0.1.0`）已替换为仓库内 vendored 的 `libvault`（`libvault/` 目录，path 依赖，非 workspace member）。新依赖的能力面与旧版不同，本文相关阶段已据此核查与修订，主要影响：
> - **阶段 I（root token 退役 / 最小权限）**：libvault 已原生提供 ACL policy（`modules/policy`，`sys/policy/{name}`）与非 root token（`modules/auth/token_store.rs`，`auth/token/create`），本阶段从"自建授权体系"改为"接入并编排内建能力"——可行性提升。
> - **阶段 H（审计）**：libvault 的 `sys/audit` 仅为桩实现（handler 返回 `Ok(None)`，见 `modules/system/mod.rs:883-905`），审计须在收窄后的 `VaultCoreInterface` 上做 hook，**不能**依赖内建审计设备。
> - **阶段 J（轮换/rekey）**：unseal 分片 rekey（`generate_unseal_keys()`，`core.rs:591`）与一次性解封（`unseal_once()`，`core.rs:534`）可用；但 **KEK 轮换无内建原语**（`init()` 后 KEK 不可变，无 `sys/rotate` 等价能力），需另立专项或暂不承诺。
> - **PKI**：新 libvault 的 PKI 按证书类型分域（`tls`/`ssh`/`pgp`），调用路径已变（如 `pki/root/tls/generate/internal`、`pki/ca/tls/pem`），`src/vault/pki.rs` 已随迁移同步。

## 当前实现概览

`vault` 模块位于 `src/vault/`，核心集成代码在 `src/vault/integration/`：

- `src/vault/integration/vault_core.rs`：封装 `libvault::RustyVault`（仓库内 vendored 的 path 依赖），提供 `VaultCore` 和 `VaultCoreInterface`。
- `src/vault/integration/jupiter_backend.rs`：将 RustyVault 的物理存储后端适配到 `jupiter` 数据库存储。
- `src/jupiter/storage/vault_storage.rs`：通过 SeaORM 读写 `vault` 表，提供 `list_keys`、`load`、`save`、`delete`、`delete_all`。
- `src/context/mod.rs`：在 `AppContext::new` 中构造 `Storage`、Redis 连接、`VaultCore`，随后在 vault 之后启动 mail/notification dispatcher。
- `src/server/ssh_server.rs`：通过 vault 保存或读取 `ssh_server_key`。
- `src/vault/pgp.rs`、`src/vault/nostr.rs`、`src/vault/pki.rs`：基于 `VaultCore` 扩展 PGP、Nostr、PKI 能力。

当前 vault 已经是可用的通用 KV secret store。`VaultCoreInterface` 提供：

- `read_secret(name)`：读取 `secret/<name>` 下的 KV 数据。
- `write_secret(name, data)`：写入 `secret/<name>`。
- `delete_secret(name)`：删除 `secret/<name>`。

因此，后续把可迁移凭据写入 vault 时，不需要重新发明 secret 存储能力。真正需要补齐的是安全加固、初始化语义、错误模型、最小 bootstrap、运维命令和配置侧的 `SecretRef` resolver。

## 当前实现状态速览表（2026-06-15）

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
| --- | --- | --- |
| RustyVault 集成 | 已实现 | `VaultCore` 封装仓库内 vendored `libvault`，通过 `JupiterBackend` 落到 DB。 |
| KV secret 读写删 | 已实现 | `read_secret` / `write_secret` / `delete_secret` 已可用，但调用方直接传相对 name，缺少类型校验。 |
| vault 物理后端 | 已实现 | `JupiterBackend` 依赖完整 `Storage`，实际只需要 `VaultStorage`，阻塞最小 DB/Vault bootstrap。 |
| `core_key.json` 自动解封 | 已实现（高风险） | 分片与 root token 明文同文件落盘；缺 key 时会 `delete_all()` 清库重建。 |
| root token 生命周期 | 未实现 | 常规读写仍用 root token；非 root token、policy 编排尚未接入 monoengine。 |
| secret 访问审计 | 未实现 | libvault 内建 `sys/audit` 是桩实现，必须在收窄后的 interface 上做 hook。 |
| 最小 DB/Vault bootstrap | 未实现 | `config secret set/check` 不能安全复用完整 `AppContext`。 |
| `LoadMode` / `SecretRef` / resolver | 未实现 | 需要与 `config.md` 协同；在其就绪前，`mail.password_ref` 不能作为可执行交付。 |

## 启动依赖顺序

当前服务启动的关键顺序如下：

```text
Config::new
  -> Storage::new(config)          # 建数据库连接、构造对象存储、初始化部分存储能力
  -> init_connection(redis)        # 连接 Redis
  -> VaultCore::new(storage)       # vault 此时才就绪
  -> SmtpMailer + EmailDispatcher  # mail 启用时，vault 之后启动邮件 outbox dispatcher
  -> init_monorepo
  -> HTTP / SSH / multi 服务分发
```

该顺序形成硬约束：

- `database.db_url` / 数据库密码属于引导配置，不能进入本项目 vault。
- `redis.url` 当前在 vault 前被消费，暂时不能进入本项目 vault。
- `object_storage.s3.access_key_id` / `secret_access_key` 当前在 `Storage::new` 中、vault 前被消费，暂时不能进入本项目 vault。
- `mail.password` 的消费晚于 vault 就绪；当前在 `AppContext::new` 中构造 `SmtpMailer` 并启动 `EmailDispatcher`，是第一批较合理的可迁移凭据。
- `config secret set/check`、`config validate --resolve-secrets` 不能复用完整 `AppContext`，必须使用最小 DB/Vault bootstrap。

任何试图在 `Config::new` 中读取 vault secret 的方案都不可行，因为 `Config::new` 是同步加载阶段，且此时 vault 还没有就绪。

## Vault 初始化时序（按代码核实）

本节是上面“启动依赖顺序”的代码级展开，所有步骤都标注了文件与行号，供实现与评审核对。启动分为两个阶段：**同步阶段**（尚未创建 tokio runtime，vault 不可能就绪）与**异步阶段**（`service::exec` 的 `#[tokio::main]` 启动 runtime 之后）。

### 进程入口到 vault 就绪

```text
【同步阶段 · 无 runtime】
main()                                          src/main.rs:34
└─ cli::parse(None)                             src/cli.rs:21
   ├─ ConfigLoader::new(input).load()           src/cli.rs:35   解析配置路径
   ├─ Config::new(path)                         src/cli.rs:37   同步加载 TOML（vault 未就绪）
   ├─ init_log(&config.log)                     src/cli.rs:44   ★ 安装 tracing subscriber（早于 vault）
   ├─ ctrlc::set_handler                         src/cli.rs:52
   └─ exec_subcommand(config, "service", ..)    src/cli.rs:65 → builtin_exec → service::exec

【异步阶段 · #[tokio::main] 启动 runtime】       src/commands/service/mod.rs:24
service::exec(config, args)
└─ AppContext::new(config).await               src/context/mod.rs:24
   ├─ Storage::new(config)                      src/context/mod.rs:33
   │   ├─ database_connection(&database)        src/jupiter/storage/mod.rs:191  建 DB 连接池
   │   ├─ ObjectStorageFactory::build(..)       src/jupiter/storage/mod.rs:206  ★ 对象存储（vault 前）
   │   └─ init_default_sidebars(&sidebar)       src/jupiter/storage/mod.rs:219-221
   ├─ init_connection(&config.redis)            src/context/mod.rs:36           ★ Redis（vault 前）
   ├─ VaultCore::new(storage.clone())           src/context/mod.rs:39   ◀── vault 在此初始化
   ├─ SmtpMailer + EmailDispatcher spawn        src/context/mod.rs:46-55        （vault 之后，mail 启用时）
   └─ mono_service.init_monorepo(&monorepo)     src/context/mod.rs:60-64        （vault 之后）
└─ 分发 http::exec / ssh::exec / multi::exec    src/commands/service/mod.rs:34-36
      └─（SSH 路径）读/生成 ssh_server_key       src/server/ssh_server.rs:78     ← vault 后续消费者之一
```

### Vault 内部时序（`VaultCore::new` → `config`）

```text
VaultCore::new(storage)                          src/vault/integration/vault_core.rs:52
├─ dir = mega_base()/vault ; create_dir_all      :53,56   （expect；无权限收紧）
└─ VaultCore::config(storage, key_path)          :60
   ├─ backend = JupiterBackend::new(storage)      :61   （RustyVault 物理后端 = DB 的 vault 表）
   ├─ SealConfig { secret_shares:10, secret_threshold:5 }  :62
   ├─ RustyVault::new(backend, None)              :67   （expect）
   ├─ if !key_path.exists():                      :70   ── 分支 A：首启 / key 缺失
   │    ├─ println!("clearing database…")         :71
   │    ├─ vault_storage.delete_all()             :74   ⚠ 清空 vault 表（数据丢失路径）
   │    ├─ rvault.init(&seal_config)              :79   → root_token + 10 个 shares
   │    ├─ println!("root token: {}", …)          :83   ⚠ root token 进 stdout
   │    └─ File::create(core_key.json) + 写入      :98   ⚠ 默认 umask 权限
   │  else:                                       :102  ── 分支 B：key 存在
   │    └─ read + 反序列化 CoreKey{shares,token}  :104
   ├─ for i in 0..5 { rvault.unseal(shares[i]) }  :108  解封（assert! ok）
   ├─ log::debug!("root token: {}", …)            :114  ⚠ root token 进 tracing（debug 级）
   └─ return VaultCore{ rvault, key }             :122  ◀── vault 就绪
```

### 时序图

```mermaid
sequenceDiagram
    autonumber
    participant M as main()
    participant CLI as cli::parse (同步)
    participant SVC as service::exec #[tokio::main]
    participant AC as AppContext::new
    participant ST as Storage::new
    participant RDS as Redis
    participant VC as VaultCore
    participant RV as RustyVault
    participant DB as vault 表 (JupiterBackend)
    participant MAIL as Mail/Notification
    participant SRV as http/ssh 服务

    M->>CLI: parse(None)
    CLI->>CLI: ConfigLoader.load() + Config::new()（vault 未就绪）
    CLI->>CLI: init_log() ★ tracing 安装
    CLI->>SVC: exec_subcommand → service::exec（runtime 启动）
    SVC->>AC: AppContext::new(config).await
    AC->>ST: Storage::new(config)
    ST->>DB: database_connection() 建 DB 连接池
    ST->>ST: ObjectStorageFactory::build() ★对象存储(vault 前)
    ST->>ST: init_default_sidebars()
    AC->>RDS: init_connection(redis) ★Redis(vault 前)
    AC->>VC: VaultCore::new(storage.clone()) ◀ vault 开始初始化
    VC->>VC: create_dir_all(base/vault)
    VC->>RV: RustyVault::new(JupiterBackend)
    alt core_key.json 不存在（首启/丢失）
        VC->>DB: delete_all() ⚠ 清空 vault 表
        VC->>RV: init(Seal{10,5}) → root_token + 10 shares
        VC-->>SVC: println!/log::debug! 输出 root token ⚠
        VC->>VC: 写 core_key.json（默认权限 ⚠）
    else core_key.json 存在
        VC->>VC: 读取并反序列化 CoreKey
    end
    loop 5 次 (secret_threshold)
        VC->>RV: unseal(shares[i]) assert ok
    end
    VC-->>AC: VaultCore（vault 就绪）
    AC->>MAIL: SmtpMailer::new + EmailDispatcher::new + spawn（mail 启用时）
    AC->>ST: mono_service.init_monorepo()（vault 之后）
    AC-->>SVC: AppContext
    SVC->>SRV: http::exec / ssh::exec / multi::exec
    SRV->>VC: (SSH) read_secret("ssh_server_key")｜不存在则生成并 write_secret
```

### 时序中的关键事实

- vault 就绪点是唯一的 `VaultCore::new`（`context/mod.rs:39`）；在它之前已消费 DB、对象存储（`Storage::new` 内 `:206`）、Redis（`:36`）——这正是“启动依赖顺序”把这三类判为引导 / 早期运行时依赖、不能直接改 `SecretRef` 的代码依据。
- `mail.password` 的消费点（`SmtpMailer::new`，随后 `EmailDispatcher::new` + spawn）在 `context/mod.rs:46-55`，晚于 vault，是第一批可迁移凭据。当前失败路径仍会被 `if let Ok(m)` 静默忽略，需要后续可诊断化。
- `init_monorepo` 在 mail/notification dispatcher 启动之后、服务分发之前执行（`context/mod.rs:60-64`）。
- tracing subscriber 在 `cli.rs:44` 就已安装，早于 vault；因此 `VaultCore::config` 的 `println!`（:71/:83/:93/:103）与 `log::debug!(root_token)`（:114）会真的把 root token / 分片写进 stdout 与日志（详见“当前主要问题 · root token 明文输出”）。
- 分支 A 的 `delete_all()`（:74）仅凭 `core_key.json` 不存在即触发，无法区分“全新空库首启”与“误删 key 但库内有数据”（详见“fail-closed 判定依据必须区分首次初始化与误删 key”）。

## 威胁模型与安全边界

本计划的所有加固动作都应服务于一个明确的威胁模型；缺少威胁模型时，“是否符合 Vault 安全标准”无法被验证。

资产：

- vault 加密主密钥与 unseal 分片（`core_key.json` 中的 `secret_shares`）。
- vault root token。
- vault 内业务 secret（SSH host key、PGP / Nostr / PKI 私钥，以及未来迁移的 `mail.password` 等）。

假定的攻击者与防护目标：

- 仓库与代码评审泄露：secret 不得进入 git、TOML、镜像层。可防护。
- 日志与可观测性泄露：root token、分片、secret 明文不得进入 stdout / stderr / tracing / 错误信息。可防护（阶段 A）。
- 应用层误用：调用方不应接触 root token，也不应自由拼 raw path。可防护（阶段 C）。
- 进程内存读取：同一进程内 secret 必然以明文存在，不在本计划防护范围。
- 可读取部署机磁盘的攻击者：当前自动解封把分片与 root token 落盘，磁盘读取即等于完全攻陷 vault。**当前实现不防护此攻击**，只能通过外部 KMS / transit 自动解封或 systemd / K8s 注入降低，属阶段 J 与部署侧职责。

残余风险（必须在文档与运维说明中显式声明）：

- 自动解封模式下，`secret_shares: 10 / secret_threshold: 5` 的 Shamir 分片不提供职责分离收益——全部 10 份分片与 root token 同处一个 `core_key.json`，并在启动时自动用其中 5 份解封。分片在当前形态下是“安全装饰”；真正的职责分离需要把 key material 交给外部 KMS / transit seal，或多人托管分片（与无人值守启动互斥）。libvault 提供 `unseal_once()`（`core.rs:534`）一次性解封原语，可在外部 / 人工解封场景下"用后即废"分片以防重放，但对"分片落盘后自动解封"这一当前形态**无帮助**——后者仍只能由外部 KMS / transit 缓解。
- 因此安全收益的准确表述是“不进仓库、不进普通配置、不进日志、不被普通应用调用方接触”，而**不是**“可抵御获得磁盘读权限的攻击者”。

本期对齐的 Vault 标准子集：fail-closed seal、最小权限与 root token 生命周期、secret 访问审计、轮换 / rekey、脱敏、备份恢复。明确不在本期范围（如需可另立计划）：动态 secret 与租约 TTL、transit 加密即服务、response wrapping、HSM 集成。

## 当前主要问题

### `core_key.json` 缺失会清空 vault

`src/vault/integration/vault_core.rs` 当前在 key 文件不存在时会：

1. 打印清库重建提示。
2. 调用 `vault_storage.delete_all()` 清空 vault 表。
3. 重新初始化 RustyVault。
4. 生成新的 root token 和 secret shares。
5. 写入新的 `core_key.json`。

这意味着 `core_key.json` 丢失会导致所有 vault secret 被删除。集中更多凭据后，这会变成灾难性数据丢失路径。普通启动和 `config secret check` 必须改为 fail-closed：key 缺失时启动失败并提示恢复或显式 reset，不能自动清库。

当前测试 `test_vault_reinitialize_after_file_loss` 还固化了这一危险行为，后续需要改为验证 fail-closed。

### root token 明文输出

`VaultCore::config` 当前会通过 stdout 和 debug log 输出 root token。这违反 secret 迁移前的基本安全前提。迁移 `SecretRef` 前必须保证：

- root token 不进入 stdout。
- root token 不进入 stderr。
- root token 不进入 tracing 日志。
- secret shares 和完整 `core_key.json` 内容也不得输出。

否则，`SecretRef` 只能避免凭据进入 TOML，不能避免凭据进入日志。

### `core_key.json` 权限未显式收紧

当前 `core_key.json` 使用 `std::fs::File::create` 创建，依赖系统默认 umask。生产前需要显式加固：

- Unix 下 vault 目录权限应为 `0700` 或等价限制。
- Unix 下 `core_key.json` 权限应为 `0600`。
- 容器镜像、备份、日志采集必须排除该文件。
- 生产部署应优先考虑 systemd credential、Kubernetes Secret volume 或外部 secret manager 注入 key material，而不是长期保存普通明文文件。

即便完成权限加固，当前自动解封模式仍不能抵御能读取部署机磁盘的攻击者。安全收益应限定为“不进仓库、不进普通配置、不进日志”。

### 初始化路径大量 panic

`VaultCore::new` / `VaultCore::config` 当前返回 `Self`，内部使用大量 `expect`、`unwrap`、`assert`。这让调用方无法区分：

- vault 目录创建失败。
- key 文件不存在。
- key 文件不可读。
- key 文件格式错误。
- RustyVault 创建失败。
- init 失败。
- unseal 失败。
- 数据库存储失败。

后续应改为返回 `Result<Self, MegaError>`，或引入 `VaultError` 后统一转换为 `MegaError`。普通服务启动、`config secret check`、`config validate --resolve-secrets` 都需要可诊断错误，而不是 panic。

### `JupiterBackend` 依赖完整 `Storage`

`JupiterBackend` 当前接收完整 `Storage`，但实际只使用 `ctx.vault_storage()`。这让 vault bootstrap 被完整 `Storage::new` 绑定，而 `Storage::new` 会提前构造对象存储等能力。

这条接口边界太粗，直接阻碍 `config secret set/check` 独立运行。目标是拆出最小 DB/Vault bootstrap，只建立数据库连接和 `VaultStorage` 所需能力，不初始化 Redis、对象存储、HTTP、SSH、monorepo 或后台任务。

### `VaultCoreInterface` 暴露过多底层细节

当前 interface 包含 `token()`、`read_api`、`write_api`、`delete_api`、`read_secret`、`write_secret`、`delete_secret`。普通调用方不应该知道 root token，也不应该直接拼 RustyVault API path。

后续应分层：

- vault 内部保留 raw API 能力。
- 业务调用方只使用规范化 secret name。
- 配置系统通过 `SecretResolver` 使用 `SecretRef`，不直接依赖 raw vault path。

### secret path 语义容易误用

`write_secret(name)` 内部会访问 `secret/{name}`。如果调用方传入 `secret/config/prod/mail/password`，最终会访问 `secret/secret/config/prod/mail/password`。

因此运维命令和 resolver 必须明确区分：

- VaultCore 的相对 secret name：`config/prod/mail/password`。
- 配置文件里的引用：`vault://secret/config/prod/mail/password#value`。
- RustyVault 内部 path：`secret/config/prod/mail/password`。

resolver 不能把完整 URI 直接传给 `read_secret`。

### 消费端假设 vault 数据永远正确

当前 SSH、PGP、Nostr 等调用点大量使用 `unwrap()` 和 `expect()`，并假设 secret JSON shape 永远正确。例如：

- `src/server/ssh_server.rs` 读取 `ssh_server_key.secret_key`。
- `src/vault/pgp.rs` 读取 `pgp-signed-secret.pub_key` / `sec_key`。
- `src/vault/nostr.rs` 读取 `nostr_identity_key.nostr` / `secret_key`。

当 vault 数据损坏、字段缺失或格式错误时，服务会 panic。迁移配置 secret 前，需要逐步把这些路径改为返回可诊断错误。

### root token 永久留存，且应用全程以 root 身份操作

`core_key.json` 永久保存 root token，`VaultCoreInterface` 的所有读写都用 `self.token()`（即 root token）直连 RustyVault。这违反 Vault 的最小权限与 root token 生命周期标准：root token 应仅用于初始化，之后撤销，日常操作通过按 policy 限权的非 root token 完成，需要时再用 generate-root 临时重建。

仅“不输出 root token”无法解决这一点：只要 root token 长期存在并用于每次操作，一旦泄露即等于完全攻陷。阶段 C 收窄的是 Rust 调用面，并非 vault 层授权——底层仍是 root。真正的最小权限需要在 vault 内定义 policy，并为 SSH、PGP、Nostr、PKI、config 等消费端签发各自的限权 token（见阶段 I）。

### 缺少 secret 访问审计

当前没有任何 secret 访问审计。集中托管凭据的系统若无法回答“谁、在何时、访问了哪个 secret、结果如何”，不满足 Vault 生产加固的基本要求。需要启用审计设备（或在 interface 上统一审计 hook），记录每次 secret 读写的元数据，并对 secret 值做哈希 / 省略；审计日志本身不得泄露明文、root token 或分片（见阶段 H）。

### 缺少轮换、rekey 与备份恢复方案

计划目前只有破坏性 re-init，缺少三类标准能力：

- vault 加密 key 轮换（`sys/rotate` 等价能力）。
- unseal 分片 rekey：`core_key.json` 疑似泄露后，在不丢数据的前提下重新生成分片与 root token。
- 业务 secret 轮换（例如周期性更换 `mail.password`）。

更关键的是，阶段 A 把 key 缺失改为 fail-closed 是正确的，但 fail-closed 的另一面是“缺少恢复路径就等于 key 丢失即永久数据丢失”。文档要求把 `core_key.json` 排除出镜像 / 日志 / 备份，却没有定义 key material 的安全托管位置与恢复流程。必须明确：分片 / root token 的安全备份位置（外部 secret manager、离线托管等）、恢复步骤，以及“DB 数据尚在但 key 丢失”时的 rekey / 恢复预案（见阶段 J）。这与 `config.md` 中“Vault 加固是 SecretRef 生产化迁移硬前置”的约束一致。

### fail-closed 判定依据必须区分首次初始化与误删 key

仅以“`core_key.json` 是否存在”做判定会引入新缺陷：全新部署本来就没有 key 文件，若一律 fail-closed，则在阶段 D 的 CLI 初始化命令落地前，全新环境将**无法完成首次初始化**。

正确判定依据是 vault 在 DB 中是否已初始化（`rvault.core.load().inited()`，测试已使用该接口），而非 key 文件是否存在：

- DB 未初始化 + key 文件缺失 → 合法首次初始化，允许 init 并写入新 key 文件。
- DB 已初始化 + key 文件缺失 → 危险（数据在、解封材料丢失）→ fail-closed，提示恢复或显式 reset，**绝不** `delete_all()`。

### `core_key.json` 与 `vault` 表缺少格式 / 兼容迁移

一旦阶段 A / I 改变 key 文件语义（例如不再长期保存 root token、或新增字段），已有部署的旧 `core_key.json` 需要兼容或迁移路径，否则升级即触发 fail-closed。计划需要为 key 文件和 `vault` 表的格式演进定义版本字段与迁移策略。

## 字段迁移分类

| 字段 | 当前消费点 | 分类 | 迁移结论 |
| --- | --- | --- | --- |
| `database.db_url` / 数据库密码 | `Storage::new` 建库连接 | 引导配置 | 不能进本项目 vault；通过 TOML/env/部署平台 secret 提供 |
| `redis.url` | `AppContext::new` 中 vault 前连接 Redis | 早期运行时依赖 | 暂时不能进本项目 vault；若含密码应走部署平台 secret 并脱敏日志 |
| `object_storage.s3.*` | `Storage::new` 中 vault 前构造对象存储 | 早期运行时依赖 | 暂时不能进本项目 vault；需先重构初始化顺序 |
| `orion_server.db_url` | Orion 相关配置 | 引导或独立服务配置 | 不默认纳入 monoengine vault；按 Orion 启动依赖单独判断 |
| `mail.password` | `AppContext::new` 中 vault 之后构造 `SmtpMailer` 并启动 `EmailDispatcher` | 可迁移凭据 | 第一批可改为 `SecretRef`；构造失败需从静默忽略改为可诊断处理 |
| `ssh_server_key` | SSH server 启动时读取或生成 | vault 内部 secret | 已由 vault 管理，但需要加固错误处理和 key 文件安全 |
| PGP / Nostr key | `vault/pgp.rs`、`vault/nostr.rs` | vault 内部 secret | 已由 vault 管理，但需要清理 panic 与数据 shape 校验 |

## 跨模块协同前置（2026-06-16 修订）

以下事项不是所有 vault 工作的统一硬前置。它们只阻塞依赖对应能力的阶段；vault 本地安全止血、最小 bootstrap、interface 收窄、消费端 panic 清理可以先行推进。

> **现状提示（2026-06-15）**：经核查，下列三项前置在当前仓库**均尚未实现**。但它们只阻塞依赖各自前置的阶段（A 的脱敏部分、D、E、G）——存在一条不依赖它们的 vault-only 执行链（A 止血子集 → B → C → F），详见下文《可执行性评估》。

### 1. 日志脱敏工具（redaction 模块）设计与实现

**目标**：建立统一的敏感信息脱敏工具，供 config、vault、mail、notification 等模块共享使用。

**位置建议**：`src/common/redaction.rs` 或 `src/config/redaction.rs`

**职责范围**：
- 脱敏 URL（移除密码和关键参数）
  - 示例：`postgres://user:password@host:5432/db` → `postgres://***:***@host:5432/db`
- 脱敏 token 和密钥
  - 示例：`secret_abc123xyz` → `secret_***`
- 脱敏 Vault 相关信息
  - root token：完整隐藏
  - secret shares：完整隐藏
  - secret 值：完整隐藏（仅保留路径）
- 脱敏配置相关信息
  - 数据库 URL 中的密码
  - Redis URL 中的密码
  - 对象存储密钥

**API 设计建议**：
```rust
pub trait Redactor {
    /// 脱敏字符串（通用脱敏策略）
    fn redact(&self, value: &str) -> String;
    
    /// 脱敏数据库 URL
    fn redact_db_url(&self, url: &str) -> String;
    
    /// 脱敏 Redis URL
    fn redact_redis_url(&self, url: &str) -> String;
    
    /// 脱敏密钥/token（假设是不可见字符或长字符串）
    fn redact_secret(&self, value: &str) -> String;
    
    /// 脱敏 JSON（递归处理，隐藏 password、token、secret 等关键字段）
    fn redact_json(&self, json_str: &str) -> String;
}

pub fn global_redactor() -> &'static dyn Redactor;
```

**在 vault 中的使用点**：
- 需要保留但可能包含敏感上下文的日志 / 错误输出：使用 `redactor` 后再输出；阶段 A 的 root token / 分片 / key 文件内容应直接删除输出，不应先构造再脱敏
- 错误信息中：使用 `redactor.redact_json()` 处理 `CoreKey` 反序列化失败的错误信息
- `VaultError` 的 `Display` 实现中：所有敏感字段都应经过脱敏

**前置条件**：
- 只阻塞“需要保留但必须脱敏的日志/错误输出”、跨模块脱敏测试与 SecretRef 生产化 gate。
- 不阻塞阶段 A 中“删除 root token / 分片 / key 文件内容输出”的核心止血；这部分应直接删除敏感输出，避免先构造再脱敏。
- 可作为**独立前置工作**先完成
- 不被任何其他阶段阻塞

**验收标准**：
- 脱敏工具能被 config、vault、mail、notification 等模块导入使用
- 单元测试覆盖所有脱敏类型
- 脱敏后的输出不包含明文敏感信息（token、密码、shares 等）

---

### 2. CLI 两阶段加载框架（与 config 团队协同）

**目标**：与 config.md 阶段 2 协同设计 `LoadMode` 框架，为两个模块都需要的 CLI 改造提供统一的设计。

**设计范围**：
- 定义 `LoadMode` enum（所有可能的加载级别）
- 明确各模式的启动路径和依赖
- 定义命令与加载模式的映射关系

**期望产出**：
- 共同设计文档或 RFC
- `src/cli/load_mode.rs` 或等价的设计代码框架

**前置条件**：
- 阶段 D（P1）需要与 config 阶段 2 协同完成此设计
- 不能分别实施，否则会出现不一致

---

### 3. 与 config.md 的进度协调

**关键约束**：
- **阶段 B（最小 bootstrap）应在 config 阶段 3 之前完成**，或至少同期进行
- config 阶段 3 直接依赖 vault B 的改造结果
- 两个团队应协商明确的交付顺序和时间表，避免 config 因等待 vault 而延期

## 可执行性评估（2026-06-15 核查）

本节按"当前代码与依赖的实际状态"核查各阶段能否立即执行，仅调整可行性与排期，不改动代码。核查基于 monoengine 当前 `src/` 与仓库内 vendored `libvault`。

**可执行度总评：中高（7/10）。** vault 自身的 P0/P1 加固路径具备落地条件，主要障碍不是技术不可行，而是文档后半部分仍沿用旧前置口径：把"删除敏感输出"错误地写成必须等待 redaction，把"最小 bootstrap"与"LoadMode/SecretRef"混在同一阻塞链里。执行时必须把工作拆成两条队列：

- **vault-only 队列**：A 核心止血 → B → C → F，可立即开工；H、I 在 C 之后接入，不需要等待 `LoadMode` 或 `SecretRef`。
- **跨 config 队列**：redaction、`LoadMode`、`SecretRef`、对象存储后置初始化，按 `config.md` 对应阶段协同推进；这些工作不应反向阻塞 A 核心。

### 可执行性问题诊断

1. **前置依赖口径需保持收敛**：A 核心不依赖 redaction；文档中任何把“删除敏感输出”写成“必须等待脱敏工具”的表述都应删除或降级为“保留输出时才需要脱敏”。否则会让可以立即落地的安全止血被误判为阻塞。
2. **阶段颗粒度不够执行化**：A 同时包含删除输出、错误模型、fail-closed、权限、reset 命令设计，实际应拆成"A1 直接止血"、"A2 初始化语义"、"A3 权限与测试"三个可评审单元。
3. **跨模块工作与本地工作混排**：D/E/G 的确依赖 config，但 B/C/F/H/I 不依赖 config 交付，优先级表应显式分流。
4. **验收标准可测，但缺少入口清单**：已有验收项大多具体，但缺少"第一批 PR 改哪些文件、不能改哪些范围"的执行边界。
5. **仓库格式容易误判**：当前工作副本使用 Libra 管理，`git diff` 失败不是代码或文档不可执行的证据。执行者应先用 `libra status` 确认已有改动，再用 `libra diff -- docs/refactoring/vault.md` 或对应路径核对差异，避免误操作其他人已有改动。
6. **代码定位应以符号为准**：当前行号有轻微漂移，执行时应以 `VaultCore::config`、`JupiterBackend::new`、`VaultCoreInterface` 等符号定位，行号只作辅助。

### 关键事实（已核查）

1. 三项跨模块"硬前置"在当前仓库**尚不存在**：
   - 日志脱敏工具（config 0b）：`src/common/redaction.rs` / `src/config/redaction.rs` 不存在，无 `Redactor`。
   - CLI `LoadMode`（config 2）：`src/cli/load_mode.rs` 不存在，无 `LoadMode`。
   - `config secret set/check/ref`、`SecretRef`、`config validate --resolve-secrets`：未实现。
   - 凡声明依赖这三者的阶段（A 的脱敏部分、D、E、G），若不先交付前置，无法照原文执行。

2. libvault 内建治理路径**开箱可达**（embedded API：`init()` → `unseal()` 后用 root token 即可 `write`/`read`）：`sys/policy/{name}`、`sys/policies/acl/{name}`、`auth/token/create`、`secret/*` 均为默认挂载（`libvault/src/mount.rs:45-64`、`modules/auth/mod.rs:38-48`、`core.rs:609-632` 的 `post_unseal`）。→ 阶段 I 的 policy/token **不需要额外挂载 plumbing**，monoengine 侧即可落地。阶段 H 不依赖这些内建路径，应在 monoengine 的收窄 interface 上实现 hook。

3. fail-closed 判定依赖的 `rvault.core.load().inited()` 存在且测试已用——阶段 A 第 5 项可直接执行。

4. 当前工作副本是 Libra 格式仓库；工作区检查应使用 `libra status`，差异检查应使用 `libra diff -- <path>`。这只影响开发/评审工作流，不改变 vault 代码的技术可执行性。

5. 行号轻微漂移：本次迁移合并了 `context/mod.rs` 的 mail 分支（原文 `:46-55` 现约 `:48-57`），`vault_core.rs` 多数行号仅 ±1。**执行前应以符号（函数名 / 路径字符串）而非行号定位**。

### 可执行性分级

| 阶段 | 可执行性 | 依据 / 调整 |
|---|---|---|
| **A（止血）核心子集** | ✅ 现在可执行，无外部依赖 | "停止输出 root token / 分片 / key 文件内容"只需删除 `println!`/`log::debug!` 并停止构造含密字符串——**不需要脱敏工具**。fail-closed（`inited()`）、文件权限（`0700`/`0600`）、`Result`/`VaultError` 均为本地改造。 |
| A 中"脱敏残留日志"部分 | ⛔ 阻塞于 config 0b | 仅当需把含密 URL（DB/Redis）部分脱敏后仍输出时才需要；与"止血"解耦，不应连坐阻塞 A 的核心。 |
| **B（最小 bootstrap）** | ✅ 现在可执行 | 纯 monoengine 结构改造。 |
| **C（收窄 interface）** | ✅ 现在可执行 | 纯 monoengine 改造；阶段 H 的审计 hook 建议并入本阶段产物。 |
| **F（清理消费端 panic）** | ✅ 可执行（依赖 A/B） | PKI 路径已迁移，错误模型清理基于新路径。 |
| **H（审计 hook）** | 🟡 可执行，需先定审计落点与失败策略 | 只走 interface hook（内建审计是桩）；技术无阻塞，排在 C 之后。 |
| **I（policy + 非 root token）** | 🟡 可执行，libvault 侧已确认可达 | 内建 policy/token 开箱可达；剩余为 monoengine 编排，无须改 libvault。 |
| **J：分片 rekey / secret 轮换** | 🟡 可执行（有内建原语） | 基于 `generate_unseal_keys()` / `unseal_once()`。 |
| **D（CLI LoadMode + secret 命令）** | ⛔ 阻塞于 config 2 | `LoadMode` 不存在，须与 config 协同先交付框架。 |
| **E（SecretRef + mail.password_ref）** | ⛔ 阻塞于 config 5 / mail 2 | `SecretRef` / resolver 未实现。 |
| **G（对象存储后置）** | ⛔ 阻塞于 config 7 | 可选，最后做。 |
| **J：KEK 轮换** | ⛔ 无内建原语 | 须自建，另立专项。 |

### 可行性改进结论

- **存在一条完全不依赖 config 团队的 vault-only 执行链：A（止血子集）→ B → C → F**，可立即开工；H、I 紧随其后（libvault 已确认可达）。这条链不应被尚不存在的脱敏工具 / `LoadMode` 连坐阻塞。
- **解除阶段 A 的伪硬依赖**：把"停止输出敏感值"（现在可做）与"脱敏需保留的日志"（待 config 0b）拆为两个独立工作项，前者不再等待脱敏工具。
- **D / E / G 仍是真阻塞**：依赖 config 团队的 `LoadMode` / `SecretRef` / 对象存储重排；在它们就绪前，不要把 `mail.password_ref` 迁移列入"可执行"范围。
- **仓库格式不构成技术阻塞**：Libra 管理工作副本只改变状态/差异/提交命令；本计划的代码入口、测试命令和风险判断仍按 monoengine 源码结构执行。
- **执行前以符号定位**而非文档行号（迁移已造成轻微漂移）。

### 建议执行切片

为了避免一个 PR 同时触碰初始化语义、CLI、配置解析和业务消费端，建议按以下可回滚切片推进：

1. **A1 直接止血**：删除 root token / 分片 / key 文件内容输出，避免新增脱敏依赖；只改初始化路径和对应测试。
2. **A2 初始化语义**：`VaultCore::new/config` 返回 `Result`，fail-closed 以 DB 初始化状态为准，移除普通启动中的 `delete_all()` 数据丢失路径。
3. **A3 key material 权限**：Unix 权限 `0700`/`0600` 与权限断言测试；非 Unix 平台只验证不放宽现有行为。
4. **B 最小 bootstrap**：拆 `JupiterBackend` 的 storage 边界，但不新增 `config secret` 命令。
5. **C/H interface 与审计入口**：先隐藏 token/raw path，再挂审计 hook；审计目的地与 fail-open/fail-closed 策略须在实现前明确。
6. **F 消费端错误模型**：SSH、PGP、Nostr、PKI 分批清理 panic，不与 SecretRef 迁移混在同一 PR。

### 暂不可执行边界

- 不实现 `mail.password_ref`，直到 `SecretRef` / resolver 与 mail 消费形态就绪。
- 不实现 `config secret set/check`，直到 `LoadMode` 与最小 bootstrap 都就绪。
- 不把 DB、Redis、S3 凭据迁入本项目 vault，除非先完成对应启动顺序重排。
- 不承诺 KEK 轮换，除非另立 libvault 数据重加密专项。

## 改进原则

1. 先加固 vault，再迁移配置 secret。
2. `Config::new` 只解析 `SecretRef`，不读取真实 secret。
3. secret 真实值解析必须发生在 vault 就绪之后。
4. `config secret set/check` 必须使用最小 DB/Vault bootstrap。
5. 引导配置和早期运行时依赖不进入本项目 vault。
6. root token、secret shares、明文 secret、数据库 URL 密码、Redis URL 密码都不得进入日志或错误信息。
7. 破坏性 reset 必须是显式运维动作，不能隐藏在普通启动中。
8. fail-closed 必须与备份恢复方案配套；引入 fail-closed 前后，key 丢失（数据在）必须有文档化的恢复或安全重置路径。
9. 最小权限优先：root token 仅用于初始化，常规运行通过限权 token；调用方不接触 root token，也不直接拼 raw path。
10. secret 访问可审计：read/write/delete 留下不含明文的审计记录。
11. 每个阶段都应独立可编译、可测试、可回滚。

## 分阶段计划

> **阶段依赖声明（2026-06-15 修订）**：本计划不再把所有阶段统一挂到同一组跨模块前置上。执行时按以下边界处理：
> 1. **vault-only 阶段**：A 核心、B、C、F 可在当前仓库直接执行；H、I 在 C 之后执行；这些工作不等待 redaction、`LoadMode` 或 `SecretRef`。
> 2. **redaction 依赖**：只阻塞“需要保留但必须脱敏的日志/错误输出”、跨模块脱敏测试，以及后续 SecretRef 生产化 gate；不阻塞 A 核心中“删除敏感输出”的工作。
> 3. **CLI LoadMode 依赖**：只阻塞阶段 D 的 `config secret` 命令族和 `validate --resolve-secrets`，应与 `config.md` 阶段 2 共用同一框架。
> 4. **SecretRef 依赖**：阶段 E 必须等待 `config.md` 阶段 5 与 `mail.md` 阶段 2；在此之前不得把 `mail.password_ref` 列入可交付范围。
> 5. **阶段 B 的同步要求**：最小 DB/Vault bootstrap 必须在 `config.md` 阶段 3 之前完成，或与其同期交付，因为 config 的 secret 命令直接依赖该能力。

### 阶段 A：Vault 安全止血

目标：在迁移任何配置 secret 前，消除最危险的泄露和数据丢失路径。

> **前置依赖（2026-06-15 修订，见《可执行性评估》）**：工作项 1-2"停止输出 root token / 分片 / key 文件内容"**不需要**脱敏工具——直接删除 `println!` / `log::debug!`、不构造含密字符串即可，**现在可立即执行**。脱敏工具（config 0b）只在"需保留输出的含密日志（如 DB/Redis URL）做部分脱敏"时才需要；该部分与"止血"解耦，不阻塞本阶段核心。

工作项：

1. 删除 root token 的 stdout、stderr、tracing 输出。
2. 删除 secret shares 和完整 key 文件内容的任何输出。
3. `VaultCore::new` / `VaultCore::config` 改为返回 `Result<Self, _>`，引入专门的 `VaultError`（区分目录创建、key 缺失、key 不可读、格式错误、init、unseal、存储失败），并统一转换为 `MegaError`。`VaultError` 与其 `Display` 不得嵌入 root token、分片或 secret 明文；不要用 `MegaError::Other(format!(..))` 直接拼接敏感值。
4. 替换初始化路径上的 `expect`、`unwrap`、`assert`。
5. fail-closed 判定以 DB 是否已初始化（`rvault…inited()`）为准，而非 key 文件是否存在：DB 已初始化但 key 文件缺失时启动失败、**绝不** `delete_all()`；仅当 DB 未初始化且无 key 文件时才允许合法首次初始化。
6. 显式 reset/init 作为后续运维命令设计，不能由普通启动隐式触发。
7. Unix 下创建 vault 目录时设置 `0700`，创建 `core_key.json` 时设置 `0600`。
8. 将 `test_vault_reinitialize_after_file_loss` 改为验证 key 缺失不会清空 vault 数据。

验收标准：

- 启动和测试日志中不出现 root token（应有捕获日志的测试断言 token 与分片不出现）。
- 全新（空 DB）环境仍可完成首次初始化；DB 已初始化但删除 `core_key.json` 后，普通启动失败且 vault 表数据不被清空。
- 初始化失败返回可诊断错误，不 panic，且错误信息不含 token / 分片 / 明文。
- key 文件和目录权限符合最小可读写范围。

### 阶段 B：拆出最小 DB/Vault bootstrap 接口边界

目标：让 vault 运维命令不依赖完整 `AppContext`。

> **与 config.md 的强绑定（2026-06-14 更新）**：本阶段的改造结果是 config.md 阶段 3 的直接依赖。config 需要基于本阶段拆出的最小 bootstrap 能力来实现 `config secret set/check` 等命令。因此本阶段应**在 config.md 阶段 3 之前完成，或至少同期进行**，以避免 config 因等待而延期。

工作项：

1. 将 `JupiterBackend` 从依赖完整 `Storage` 改为依赖最小 vault storage 接口边界。
2. 新增 `VaultBackendStorage` 或等价接口，只覆盖 `list_keys`、`load`、`save`、`delete`。
3. 让 `VaultStorage` 成为生产 adapter。
4. 新增 DB-only / Vault-only bootstrap 能力，只建立数据库连接和 `VaultStorage`。
5. 确保该 bootstrap 不初始化 Redis、对象存储、HTTP、SSH、monorepo、后台任务。

验收标准：

- 可以只凭数据库配置构造 `VaultCore`。
- `config validate --resolve-secrets` 的底层 bootstrap 不依赖 Redis/S3。
- vault 集成测试不需要完整服务上下文。

### 阶段 C：收窄 Vault interface

目标：隐藏 token、raw API path 和路径拼接规则，减少调用方误用。

工作项：

1. 将 raw `read_api` / `write_api` / `delete_api` 限制在 vault 内部。
2. 普通业务调用方只使用相对 secret name。
3. 引入 `SecretName` 或等价校验，禁止以 `/` 开头，禁止带 `secret/` 前缀。
4. 错误信息只输出 redacted path，不输出 secret 明文。
5. 逐步让 SSH、PGP、Nostr、PKI 调用点使用收窄后的 interface。

验收标准：

- 普通调用方无法访问 root token。
- `secret/secret/...` 这类路径重复可以在入口被拒绝。
- 错误信息不包含明文 secret。

### 阶段 D：CLI LoadMode 与 vault 运维命令基础

目标：为 `config secret` 命令提供正确启动模型。

> **与 config.md 的协同设计（2026-06-14 更新）**：本阶段的工作项 1（在 CLI 层支持两阶段加载和 LoadMode）是与 config.md 阶段 2 的**跨模块协同改造**，而非独立实施。两个文档都发现了相同的需求，应作为单一设计完成。建议：
> - 先由 config + vault 团队协同设计 `LoadMode` 框架（定义 enum、各模式的启动路径、依赖关系等），输出为共同文档或代码（如 `src/cli/load_mode.rs`）
> - config.md 阶段 2 和 vault.md 阶段 D 都基于这个共同框架来实现各自的子命令
> - 避免分别实施导致的设计不一致或集成冲突

工作项：

1. **先在 CLI 层支持两阶段加载和 `LoadMode`**（与 config.md 阶段 2 协同完成）：在共同的 `LoadMode` 设计框架下，改造 CLI 分发逻辑以支持不同的启动模式。
2. 新增不依赖 vault 的 `monoengine config secret ref`。
3. 基于最小 DB/Vault bootstrap 新增 `config secret set`。
4. 基于最小 DB/Vault bootstrap 新增 `config secret check`。
5. 新增 `config validate --resolve-secrets`。

命令规则：

- `secret ref` 只生成引用，不连接数据库，不初始化 vault。
- `secret set/check` 只连接数据库和 vault，不构造完整 `AppContext`。
- `secret set` 默认只接受 `--value-stdin` 或隐藏输入。
- `secret set` 默认不修改 TOML，只输出 `vault://secret/...#field`。
- 不允许通过这些命令写入数据库密码、Redis URL、当前阶段对象存储 key。

验收标准：

- 坏配置时 `config validate` 能输出诊断，而不是被 CLI 预加载拦截。
- 缺 Redis/S3 时仍可执行需要 vault 的 secret 检查，只要数据库和 vault key 可用。
- secret 明文不进入 shell 参数、日志或错误信息。

### 阶段 E：SecretRef 与第一批配置凭据迁移

目标：让配置文件保存 secret 引用，而不是保存可迁移凭据明文。

首批字段：

- `mail.password` -> `mail.password_ref`

工作项：

1. 在配置模块中定义 `SecretRef`。
2. 定义 `SecretResolver` trait。
3. 实现 `VaultSecretResolver` adapter，内部调用 `VaultCoreInterface::read_secret`。
4. resolver 将 `vault://secret/config/prod/mail/password#value` 映射为 `read_secret("config/prod/mail/password")`，再读取 `value` 字段。
5. `mail.password` 和 `mail.password_ref` 迁移期互斥：同时存在为 hard error。
6. 消费端在 vault 就绪后通过 resolver 获取 SMTP 密码。
7. resolver 提供缓存 TTL 和 `evict` / `evict_all`。

验收标准：

- `mail.password_ref` 可解析并用于 SMTP mailer。
- secret 缺失、字段缺失、引用格式错误均返回可诊断错误。
- 错误和日志只出现脱敏引用，不出现明文密码。
- `database`、`redis`、`object_storage.s3.*` 没有被错误迁移为 `SecretRef`。

### 阶段 F：清理现有 vault 消费端 panic

目标：让 vault 内部 secret 数据损坏时可诊断、可恢复，而不是直接 crash。

优先处理：

1. `src/server/ssh_server.rs`：SSH server key 读取、生成、写入失败返回错误。
2. `src/vault/pgp.rs`：PGP key 读取、解析、保存、删除返回 `Result`。
3. `src/vault/nostr.rs`：Nostr key 读取、生成、解析返回 `Result`。
4. `src/vault/pki.rs`：PKI API 统一错误模型，避免新增 panic 风格路径。**注意（2026-06-15）**：新 libvault 的 PKI 已按证书类型分域，调用路径随迁移更新为 `pki/root/tls/generate/{internal|exported}`、`pki/roles/tls/{name}`、`pki/issue/tls/{role}`、`pki/ca/tls/pem` 等（旧 `pki/root/generate/...`、`pki/roles/...`、`pki/issue/...`、`pki/ca/pem` 已不再支持，会返回 "Logical backend path not supported"）；错误模型清理应基于新路径，且 `pki.rs` 文档注释中的 `libvault::modules::pki::*` 模块引用也已随之更新。

验收标准：

- vault secret JSON 缺字段时不会 panic。
- secret 内容格式错误时返回包含 secret name 和字段路径的错误。
- 错误信息不包含 secret 明文。

### 阶段 G：对象存储等早期依赖后置初始化（可选）

目标：只有在确实需要让对象存储凭据进入 vault 时，才重构完整初始化顺序。

目标链路：

```text
Config::new
  -> DbStorage::new(database 引导配置)
  -> VaultCore::new(db_storage)
  -> SecretResolver::new(vault)
  -> resolve object storage secrets
  -> Full Storage / server / task 初始化
```

验收标准：

- S3/S3-compatible 凭据迁移前，服务启动链路不再在 vault 前构造对象存储。
- 缺少对象存储 secret 时返回可诊断错误。
- `config secret set/check` 对其他 secret 的操作不依赖对象存储可用。

### 阶段 H：Secret 访问审计

目标：让 vault secret 的访问可追溯，满足集中凭据托管的审计要求。

> **新架构修订（2026-06-15）**：libvault 的内建审计设备**不可用**——`sys/audit`、`sys/audit/{path}` 路径虽已注册，但 handler 全部是桩实现（返回 `Ok(None)`，见 `modules/system/mod.rs:883-905`）。因此本阶段**只走 interface hook 路线**，不依赖内建审计设备（除非愿意先补实现 libvault 的这些 handler）。审计 hook 与阶段 C 强耦合，应作为阶段 C 收窄 `VaultCoreInterface` 的产物之一。

工作项：

1. 在收窄后的 `VaultCoreInterface` 上增加统一审计 hook（**不要**依赖 libvault 内建审计设备，理由见上）。
2. 记录每次 read / write / delete 的调用方、规范化 secret name、时间、结果（成功 / 失败 / 未命中）。
3. 审计记录对 secret 值做哈希或省略，绝不落明文；root token、分片不进入审计。
4. 显式决定并记录审计写入失败的策略（fail-open 还是 fail-closed）。

验收标准：

- 每次 secret 访问产生一条不含明文的审计记录。
- 审计记录包含足以定位调用方与 secret name 的字段。
- 审计目的地可配置，默认开启。

### 阶段 I：root token 退役与最小权限 policy

目标：从“全程 root”过渡到按 policy 限权，符合 Vault root token 生命周期标准。

> **新架构修订（2026-06-15）**：libvault 已内建可直接接入的原语，本阶段从"自建授权体系"改为"接入并编排内建能力"：
> - **ACL policy**：`modules/policy`，`sys/policy/{name}` 与 `sys/policies/acl/{name}`，capability 含 `deny`/`read`/`write`/`list`/`sudo`/`create`/`delete` 等，支持 HCL。
> - **非 root token**：`modules/auth/token_store.rs`，`auth/token/create` 支持 policy 子集校验、`ttl`/`explicit_max_ttl`/`period`/`num_uses`，并有 `auth/token/revoke[-orphan]`、`renew` 与后台 `ExpirationManager`（`expiration.rs`）做租约过期。

工作项：

1. 通过 `sys/policy/{name}`（或 `sys/policies/acl/{name}`）按 secret 前缀定义 ACL policy（ssh、pgp、nostr、pki、`config/*` 各自最小权限）。
2. 通过 `auth/token/create` 为各消费端签发带对应 policy、受限 TTL 的非 root token，替换直接使用 root token 的路径；单一 token 泄露的影响面由其 policy 子集界定。
3. 初始化后撤销常驻 root token（`auth/token/revoke/{id}`）。**注意**：当前 libvault 公开 API **没有** Vault 式 `sys/generate-root` 在线重建仪式（root token 仅在 `init()` 时产生）。因此"需要时临时重建 root 权限"必须依赖阶段 J 的恢复托管（安全保存初始 root token，或预置一个具 root policy 的恢复 token），而不能依赖在线 generate-root。
4. `core_key.json` 不再长期保存可用 root token，仅保留恢复所需的分片（与阶段 J 的备份方案配合）。

验收标准：

- 常规运行链路不持有可用 root token。
- 任一消费端 token 泄露只影响其 policy 覆盖的 secret。
- 仍可通过显式运维流程重建 root 权限执行管理操作。

### 阶段 J：轮换、rekey 与备份恢复

目标：让泄露和 key 丢失可恢复，而非只能清库重建；为 fail-closed 配齐恢复路径。

> **新架构修订（2026-06-15）**：本阶段的能力须区分"有内建原语"与"无内建原语"两类：
> - **unseal 分片 rekey ✅ 可做**：`generate_unseal_keys()`（`core.rs:591`，用当前 KEK 重新切分分片）与 `unseal_once()`（`core.rs:534`，解封后将已用分片标记 deprecated 并自动重生、防重放），可直接封装为运维命令。
> - **vault 加密 key（KEK）轮换 ❌ 无内建原语**：KEK 在 `init()` 后不可变，crate 未提供重加密屏障 / `sys/rotate` 等价能力。真正的 KEK 轮换需自建（密封 → 以新 KEK 重加密全部数据 → 重切分分片，本质等价于一次受控迁移，`Core::migrate()` 仅能搬运后端数据、不等于轮换），应**另立专项**；在该专项落地前本阶段不承诺 KEK 轮换，验收标准也不应包含它。

工作项：

1. 提供 unseal 分片 rekey 的运维命令（基于 `generate_unseal_keys()` / `unseal_once()`）。vault 加密 key（KEK）轮换因无内建原语，单列为后续专项，不在本阶段交付（见上）。
2. 定义 key material（分片 / 恢复凭据）的安全托管与备份位置（外部 secret manager / 离线托管），写入运维 runbook。
3. 定义“DB 数据在、key 丢失”的恢复流程，以及疑似 `core_key.json` 泄露后的 rekey 流程。
4. 为可迁移 secret（首批 `mail.password`）提供轮换支持。
5.（可选，长期）评估外部 KMS / transit auto-unseal，替代本地落盘自动解封，缓解磁盘读取威胁。

验收标准：

- 分片 rekey 后旧 `core_key.json` 失效，数据不丢。
- 文档化的恢复 runbook 可在 key 丢失（数据在）场景下恢复访问或安全重置。
- secret 轮换不需要重启全部依赖该 secret 的服务，或明确其重启要求。

### 与其他文档的协调关系（2026-06-15 更新）

| vault 阶段 | 主要工作 | 对 config 的依赖 | 对 mail 的依赖 | 对 notification 的依赖 |
|----------|--------|------------|-----------|-----------------|
| **A** (P0) | 安全止血 | 核心止血无依赖；仅“保留输出的脱敏”依赖 config 0b | 支持后续日志脱敏 | 支持后续日志脱敏 |
| **B** (P1) | 最小 bootstrap 拆分 | → config 3 依赖此 | 无 | 无 |
| **C** (P1) | 收窄 interface + 审计 hook 入口 | 无 | 支持后续类型安全 | 支持后续类型安全 |
| **D** (P1) | CLI LoadMode | ← 与 config 2 协同 | 支持后续运维 | 支持后续运维 |
| **E** (P2) | SecretRef 迁移 | 与 config 5 协同 | mail 作为第一消费者 | 后续支持 |
| **F/H/I/J** (P1-P4) | 消费端错误模型、审计、最小权限、rekey/恢复 | F/H/I/J 的 vault-only 子集无 config 前置；secret 轮换与 SecretRef 部分依赖 E | 无 | 无 |
| **G** (P4) | 对象存储后置初始化 | ← 依赖 config 7 或同等初始化顺序重排 | 无 | 无 |

**关键同步点：**
1. **日志脱敏工具（来自 config 0b）→ vault 中需要保留的敏感上下文输出**：不阻塞 A 核心止血；阻塞跨模块统一脱敏、错误诊断脱敏和 SecretRef 生产化 gate。
2. **vault B 完成 → config 3 依赖**：config 必须等待 vault 的最小 bootstrap 拆分，或与其在同一改造中交付。
3. **CLI LoadMode 框架（config 2 与 vault D 协同）**：两个文档需共同设计而非分别实施。
4. **config 5 + mail 2 → vault E**：`SecretRef` 和 `mail.password_ref` 必须等 resolver 与 mail 侧消费形态就绪后再落地。

## 推荐优先级

| 优先级 | 工作 | 原因 | 前置 |
| --- | --- | --- | --- |
| **P0** | 删除 root token / 分片 / key 文件内容输出，初始化返回 `Result` / `VaultError`（不泄敏） | 迁移任何 secret 前的安全止血；直接删除敏感输出即可 | 无 |
| **P0** | 以”DB 是否已初始化”为准的 fail-closed，替换清库重建的行为和测试 | 旧行为会灾难性丢数据；纯文件判定又会误伤全新部署 | 无 |
| **P0** | `core_key.json` 与 vault 目录权限收紧 | 与 fail-closed 同属 key material 加固，改动局部且可测 | 无 |
| **P0 并行** | 建立日志脱敏工具（可作为独立前置） | 阻塞“保留但脱敏输出”、跨模块错误诊断与后续 SecretRef 生产化，不阻塞 A 核心 | 无（独立） |
| **P1** | 拆出最小 DB/Vault bootstrap 接口边界 | `config secret set/check` 与 config 3 的必要条件；应在 config 3 之前或同期完成 | 阶段 A 核心 |
| **P1** | 收窄 vault interface，隐藏 token 和 raw path，并预留审计 hook | 降低误用和泄露风险；阶段 H 依赖统一入口 | 阶段 A/B |
| **P1** | Secret 访问审计（阶段 H） | 集中凭据托管的审计基线；只能走 interface hook | 阶段 A/B/C |
| **P1** | 清理 SSH/PGP/Nostr panic | 提升 vault 数据损坏时的可恢复性，减少 SecretRef 迁移前的 crash 面 | 阶段 A/B/C |
| **P1** | root token 退役与最小权限 policy（阶段 I） | 避免”全程 root”，符合 root token 生命周期 | 阶段 A/B/C/H |
| **P1** | key 丢失 / 泄露的备份恢复 runbook（阶段 J 第 2–3 项） | fail-closed 必须配套恢复路径 | 阶段 A |
| **P1 协同** | 设计 CLI LoadMode 框架（与 config 协同） | 阶段 D 与 config 2 的必要条件 | 无（协同） |
| **P2** | `config secret ref/set/check` | 形成标准运维入口 | 阶段 B、D + config 3/4 |
| **P2** | `SecretRef` + resolver + `mail.password_ref` | 第一批可迁移凭据 | 阶段 A/B/C/H/I + config 5 + mail 2 |
| **P3** | unseal 分片 rekey 与 secret 轮换（阶段 J 第 1、4 项） | 泄露后可恢复，不必清库重建；分片 rekey 有内建原语 | 阶段 A/B/E |
| **P4** | KEK 轮换专项（无内建原语，需自建重加密流程） | crate 未提供 `sys/rotate` 等价能力，须单独立项 | 阶段 A/B/J |
| **P4** | 对象存储后置初始化 | 只有 S3 凭据要进 vault 时才需要 | 阶段 A/B + config 7 |
| **P4** | 外部 KMS / transit auto-unseal | 缓解磁盘读取威胁，需部署侧支持 | 阶段 A/B/J |

## 架构 deepening 机会

### 将 VaultCore deepening 为 secret store module

涉及文件：

- `src/vault/integration/vault_core.rs`
- `src/vault/pgp.rs`
- `src/vault/nostr.rs`
- `src/server/ssh_server.rs`

问题：调用方需要知道 token、path、JSON shape、字段名和错误行为，interface 过宽且接近 implementation。

方案：让 `VaultCore` 对外只提供规范化 secret 操作，业务模块通过 typed helper 读取自己的 secret。path 映射、redaction、错误分类集中在 vault module 内部。

收益：调用方 interface 更小，测试 surface 更清晰，secret shape 变更的 locality 更好。

### 将 JupiterBackend 改为最小 storage 接口边界

涉及文件：

- `src/vault/integration/jupiter_backend.rs`
- `src/jupiter/storage/vault_storage.rs`
- `src/context/mod.rs`

问题：vault 只需要 vault table，却依赖完整 `Storage` 生命周期。

方案：引入最小 `VaultBackendStorage` 接口边界，让 `VaultStorage` 成为 adapter。`JupiterBackend` 不再知道完整 `Storage`。

收益：`config secret` 命令不被 Redis、S3、monorepo 初始化阻塞；vault 测试更容易隔离。

### 将 SecretRef resolver 放在配置 module

涉及文件：

- 未来 `src/config/secret.rs`
- `src/vault/integration/vault_core.rs`

问题：`VaultCore` 管 secret 存储，`SecretRef` 管配置引用，两者语义不同。

方案：在 config module 定义 `SecretRef` 和 `SecretResolver`，由 `VaultSecretResolver` 作为 adapter 连接 vault。

收益：`Config::new` 继续只做同步配置解析，secret 解析作为 vault 就绪后的独立异步阶段，符合启动依赖顺序。

### vault 运维命令使用 LoadMode

涉及文件：

- `src/cli.rs`
- `src/commands/mod.rs`
- 未来 `src/commands/config.rs`

问题：当前 CLI 在分发任何子命令前会加载完整配置，完整 `AppContext` 又会初始化过多依赖。

方案：先实现两阶段 CLI 和 `LoadMode`，再让 `config secret` 选择 `VaultBootstrap`，让 `service` 选择 `FullAppContext`。

收益：无配置、坏配置、缺 Redis/S3 时仍能执行配置诊断和 secret 运维。

### 评估 PKI 原生 ssh/pgp 能力替代 KV 裸私钥存储（新架构机会，2026-06-15）

涉及文件：

- `src/vault/pgp.rs`
- `src/vault/nostr.rs`
- `src/server/ssh_server.rs`
- `libvault/src/modules/pki`

问题：当前 SSH host key、PGP、Nostr 私钥都以 KV secret（`read_secret` / `write_secret`）形式存储并各自手工管理 JSON shape。新 libvault 的 PKI 已原生支持 `tls` / `ssh` / `pgp` 三类证书的签发、存储与吊销（`issue/ssh/*`、`roles/ssh/*`、`revoke/(tls|ssh|pgp)`、`certs/(tls|ssh|pgp)/*` 等），具备角色、序列号、CRL 等生命周期能力。

方案：评估把 SSH / PGP 由"KV 存裸私钥"迁移为"PKI 原生签发与托管"，统一证书生命周期（吊销、CRL、序列号、审计）。Nostr 若无证书语义则维持 KV。此为**可选**架构机会，非阶段 A–J 的硬性前置；若采纳应在阶段 F / I 之后单独立项，并评估对现有已存储 key 的兼容迁移路径。

收益：证书的吊销、CRL 与审计由 vault 统一管理，减少各 `vault/*.rs` 自管 JSON shape 的分散逻辑与 panic 面。

## 下一步建议

第一批 PR 应只做 P0，范围控制在 vault 安全止血：

1. `VaultCore::new` / `VaultCore::config` 改为返回 `Result`，引入不泄敏的 `VaultError`。
2. 删除 root token、分片、key 文件内容的所有输出（stdout / stderr / tracing）。
3. fail-closed 以 `rvault…inited()` 为准：DB 已初始化但 key 缺失即失败、不清库；空 DB 无 key 仍可合法首次初始化。
4. key 文件和目录创建时设置权限（目录 `0700`、`core_key.json` `0600`）。
5. 把 `test_vault_reinitialize_after_file_loss` 改为：DB 已初始化时缺 key 不清空数据，并新增空 DB 首次初始化用例。
6. 更新调用方（至少 `AppContext::new`、`ssh_server.rs`），让启动失败返回可诊断错误。

**首批 PR 执行边界：**

- 允许改动：`src/vault/integration/vault_core.rs`、相关 vault 单元测试、必要的 `AppContext::new` 返回错误传递、SSH 读取失败的最小适配。
- 允许新增：局部 `VaultError` / `VaultResult`、权限设置辅助函数、只覆盖初始化语义的测试用例。
- 暂不纳入：`config secret` 命令、`LoadMode`、`SecretRef` / resolver、mail password 迁移、对象存储初始化顺序重排、KEK 轮换。
- 评审重点：启动日志和错误字符串不得含 root token / 分片；DB 已初始化但 key 缺失时不得调用 `delete_all()`；空 DB 首启仍可初始化；所有新增失败路径返回错误而不是 panic。

**Libra 工作区检查：**

- 开工前运行 `libra status`，确认已有改动范围；不要回滚与本阶段无关的文件。
- 核对文档或代码差异时使用 `libra diff -- <path>`，例如 `libra diff -- docs/refactoring/vault.md`。
- 提交或评审时只纳入本阶段允许范围内的文件；若工作区已有不相关改动，应保持原样并在 PR / 交付说明中标明未触碰。

这一步完成前，不建议开始 `SecretRef`、`mail.password_ref` 或对象存储凭据迁移。备份恢复 runbook（P1）应与 fail-closed 同期或紧随其后落地——否则 fail-closed 会把“key 丢失”从“自动重建”变成“无法恢复”。审计（阶段 H）与 root token 退役（阶段 I）应在迁移更多生产凭据前完成，以满足 Vault 安全标准。
