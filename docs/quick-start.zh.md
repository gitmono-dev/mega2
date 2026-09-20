# 快速开始

[English](quick-start.md) · 中文

本文用仓库根的 [`mega2-compose.yml`](../mega2-compose.yml) 在本机拉起 mega2（trunk / storage-only）评估栈，跑通第一个闭环：HTTP clone → push `main` → API 读回。该栈直接从 **Docker Hub 拉取正式发布镜像**（`genedna/mega2:latest`），不构建源码、不需要 bootstrap、不需要 token。产品规则见 [`monorepo.md`](./monorepo.md)。

## 前置条件

- Docker Engine + Compose plugin（v2）、`git`、`curl`；全程在仓库根目录执行。
- 首次 `up` 会拉取 `genedna/mega2:latest`、PostgreSQL、Redis、RustFS 与 RustFS CLI 镜像，耗时取决于网络。

## 启动栈

```bash
docker compose -f mega2-compose.yml up -d --wait
```

`up` 之后无需任何初始化命令：`rustfs-init` 自动创建 `mega2` 桶，mega2 在服务启动时自动初始化空的 Monorepo（`main` 与顶层目录）。就绪探针是 `/api/openapi.json`。

> **这是仅限本机的匿名设置**：栈内 `push_auth=none`，clone / fetch / push 都不需要凭据，因此 9000 端口只绑定 `127.0.0.1`。不要改成 `0.0.0.0` 或经反向代理暴露；共享或公网部署必须改用 token 鉴权，见 [`deployment.zh.md`](./deployment.zh.md) 与 [`deploy-trunk.md`](./deploy-trunk.md)。

## 第一个闭环：clone → push → 读回

### 1. HTTP clone 子路径

storage-only 下对子路径 clone（不要对根 `/` 做根 clone，见 [`deploy-trunk.md`](./deploy-trunk.md) §9）：

```bash
git clone http://127.0.0.1:9000/project
cd project
```

### 2. 推送 main

monorepo 公开分支只有 `main`。匿名设置下推送不需要凭据：

```bash
echo "# hello mega2" > hello.md
git add hello.md && git commit -m "add hello.md"
git push origin main
```

推送经 MonoWriteQueue 直入 `main`，与产品 API 写共用 tip 权威（[`monorepo.md`](./monorepo.md)）。Git 客户端的 tag 推送会被拒绝；tag 管理走 HTTP API 或 `libra mega2 browser`。

### 3. API 读回与 Swagger UI

```bash
# 树对象下载（二进制流），200 即读回成功
curl -fsS -o /dev/null -w '%{http_code}\n' "http://127.0.0.1:9000/api/v1/file/tree?path=/project"

# 刚推送的文件内容
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/hello.md"
```

完整 HTTP 表面（Git smart HTTP、LFS、产品写、tags、可选 OCI `/v2`）在 Swagger UI 浏览：`http://127.0.0.1:9000/swagger-ui`（OpenAPI JSON：`/api/openapi.json`）。交互式终端浏览用 Libra 的 `libra mega2 browser`；mega2 本身不提供 Web UI。

## 观察、停止与清理

```bash
# 跟随 mega2 日志
docker compose -f mega2-compose.yml logs -f mega2

# 停止，具名卷保留 Postgres / Redis / RustFS / mega2 数据
docker compose -f mega2-compose.yml down

# 连数据卷一起删除（破坏性，清空这个本机评估实例）
docker compose -f mega2-compose.yml down -v
```

## 其它 Compose 文件的定位

仓库里还有两份 Compose，**都面向测试与开发**，不是本评估栈的替代品：

- [`docker-compose-storage-only.yml`](../docker-compose-storage-only.yml)（及 `docker-compose-storage-only.auth-none.yml` 覆盖）：从源码**构建**镜像的 trunk 实验栈，token 鉴权、需要 `service init` bootstrap，用于部署演练与 smoke。用法见 [`deployment.zh.md`](./deployment.zh.md) 与 [`deploy-trunk.md`](./deploy-trunk.md) §8。
- [`docker-compose.test.yml`](../docker-compose.test.yml)：集成测试（IT）数据面，见 [`development.md`](./development.md)。

## 下一步

- 日常使用（clone / push / API / tag / LFS）：[`user-guide.zh.md`](./user-guide.zh.md)
- 配置键、Profile、SecretRef、热加载：[`configuration.zh.md`](./configuration.zh.md)；带注释的样例见 `config/config.toml`
- 部署与运维（token 管理、形态不变式、OCI、Agent Capture）：[`deployment.zh.md`](./deployment.zh.md)、[`deploy-trunk.md`](./deploy-trunk.md)
- 本地开发与测试：[`development.md`](./development.md)
