# monoengine

> 一个基于 Rust 的 monorepo / Git 托管与服务引擎。它将若干最初来自 [Mega](https://github.com/web3infra-foundation/mega) 项目的子系统（尤其是 `callisto` ORM 实体和 `jupiter` 存储 / 迁移层）移植并扩展为一个集中的二进制 crate。

`monoengine` 是支撑 monorepo 平台的后端引擎：它通过 HTTP(S) 和 SSH 支持 Git wire 协议，将 Git 对象和 Git LFS blob 持久化到关系数据库以及可插拔的对象存储中，为上层 UI 客户端暴露 REST/OpenAPI 接口，并通过 `libvault` crate（crates.io `0.3.0`）内嵌一个密钥 / PKI 引擎，用于签名和凭据管理——该引擎的源码此前是 vendored 在 `src/vault/` 的，自 2026-08-21 起改为普通依赖，集成层仍在 `src/contract/vault/`。

## 与 monoui 联合启动（同栈联调）

`monoengine` 承担**授权**（monoengine 管理权限），`monoui`（sibling `../monoui` 的 `apps/next-app`，仓库 `gitmono-dev/monoui`，分支 `monoengine`）承担**认证**（唯一登录/注册面）。联调栈完全由本仓的 `docker-compose.test.yml` 驱动——monoui 只是被当作构建上下文拉入，**不要**使用 monoui 自带的 `docker-compose.yml`（那是独立的 tinyship profiles）。

> Compose 服务名仍为 `website-next` / `website-db-init`，隔离账户库仍名 `website`，`MEGA_OAUTH__WEBSITE_*` / `MEGA_NOTIFICATION__WEBSITE_MAIL_*` 配置键也不变——这些是 Rust 派生的配置面与 compose 内部 DNS，改名会让会话路径 fail-closed 成 401。变的只有构建上下文指向哪个仓库。

**目录关系**：`monoengine`、`monoui`、`orbit` 是**同级 sibling**，共享同一个父目录。联调栈依赖这种相对布局——本仓的 `docker-compose.test.yml` 用相对路径引用它们：

```
<父目录>/
├── monoengine/      # 本仓库（后端引擎；含 docker-compose.test.yml）
├── monoui/          # 前端 + 认证（apps/next-app 由 compose 拉入构建）
└── orbit/           # 对象存储实现（monoengine 的 path 依赖）
```

compose 中的 `build.context` 依此解析：
- `monoengine` 服务：`context: ..`（父目录，包含 `monoengine/` 与 `orbit/` 两个 path 依赖），`dockerfile: monoengine/Dockerfile`；
- `website-db-init` 与 `website-next` 服务：`context: ../monoui`，`dockerfile: apps/next-app/Dockerfile`。

因此**三个仓库必须 checkout 到同一父目录下**，且相对关系保持为 `monoengine`、`monoui`、`orbit` 平级；缺失其中任一（尤其 `../orbit`，它是 Cargo path 依赖）会让 compose/`cargo` 在依赖解析或构建阶段失败。

**前置条件**：按上表摆放三个 sibling 目录、端口 `17001` / `19180` / `15432` / `16379` 可用。首次构建会拉 `rust:1.97-bookworm` + `node:22-alpine` 并编译 monoengine release（较久）。

**启动**（推荐先 web 后 app，保证首次会话请求时 monoui 已就绪）：

```bash
# 在 monoengine 仓库根目录执行（即本 README 所在目录）
cd <monoengine 仓库根目录>

# 1) monoui 侧（web profile：建 `website` 库 + website-next）
#    首次改指 monoui 后必须带 --build：website-next 是 `pull_policy: never` 的
#    本地 tag，若机器上还留着从旧 sibling(../website) 构建的 website-next:local，
#    不带 --build 的 up 会直接复用它——联调看起来全绿，跑的却是旧仓库的镜像。
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile web up -d --build --wait website-next

# 2) monoengine 侧（app profile：postgres/redis/monoengine）
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app up -d --wait monoengine
```

`monoengine` 不声明对 `website-next` 的 `depends_on`（跨 profile 依赖会让 compose 拒绝 app-only 配置），因此分两步可保证 monoui 先健康、首次 `get-session` 不 fail-closed；也可一步 `--profile app --profile web up -d --wait`。

**拓扑**（compose 已配好）：

| 项 | 值 |
|---|---|
| monoui 宿主 | `http://127.0.0.1:17001`（容器 7001） |
| monoengine HTTP | `http://127.0.0.1:19180`（容器 8000） |
| monoengine→monoui 会话基址 | `http://website-next:7001`（`MEGA_OAUTH__WEBSITE_API_BASE_URL`） |
| 账户库 | 共享 postgres 上的独立 `website` 库（与 monoengine 业务库隔离） |
| CORS | 已含 `http://127.0.0.1:17001` |

**页面验证**：

- 连通性冒烟（从 monoengine 容器内打 monoui 会话端点）：

  ```bash
  docker compose -p monoengine-it -f docker-compose.test.yml \
    --profile app --profile web exec -T monoengine \
    curl -fsS http://website-next:7001/api/auth/get-session
  ```

- 浏览器页面闭环：打开 `http://127.0.0.1:17001` 注册/登录（Better Auth，浏览器获得 `better-auth.session_token` cookie）→ 打开 `http://127.0.0.1:19180` 访问受保护页面/API（`GET /api/v1/user`）→ 断言返回的 `username` / `website_user_id` 与 monoui get-session 一致；**无 cookie → 401**。

- 自动化栈级验收（ITW-03，需显式 `WEBSITE_IT=1`，否则软跳过）：

  ```bash
  # 在 monoengine 仓库根目录执行
  cd <monoengine 仓库根目录>
  source .env.test
  WEBSITE_IT=1 cargo test -p monoengine --test integration_website_auth -- --test-threads=1
  ```

**停止 / 清理**：

```bash
docker compose -p monoengine-it -f docker-compose.test.yml --profile app --profile web down
# 连数据卷一起清（可选）：
docker compose -p monoengine-it -f docker-compose.test.yml --profile app --profile web down -v
```

**注意**：浏览器会话 ≠ Git 凭据——登录后 Git/LFS/SSH 仍需在 monoengine `/api/v1/user` 下创建 access token 或登记 SSH key（见 `docs/refactoring/website-auth.md` §3）；`website-next` 未健康时请求按 fail-closed（401）处理，客户端重试即可。
