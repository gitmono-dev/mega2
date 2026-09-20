# 快速开始

[English](quick-start.md) · 中文

本文用仓库自带的 Compose 栈在本机拉起 mega2（trunk / storage-only），跑通第一个闭环：HTTP clone → token 推送 `main` → API 读回。产品规则见 [`monorepo.md`](./monorepo.md)；部署运维事实源见 [`deploy-trunk.md`](./deploy-trunk.md)。本文只保留最小可跑路径，不重复这两篇文档的配置键、端口表与 token 值。

## 前置条件

- Docker Compose v2、`git`、`curl`；全程在仓库根目录执行。
- 时间：镜像就绪后约 10 分钟。首次 `up` 会构建 `mega2:local` 镜像（Rust release 构建，耗时明显更长；构建定义见根目录 `Dockerfile`）。

## 启动 Compose 栈

栈内容（mega2 HTTP 9000 / SSH 2222，外加 Postgres / Redis / RustFS）与端口见 `docker-compose-storage-only.yml` 头部注释，本文不复制。

push token 经 Docker secret 文件注入（配置侧为 `${file:/run/secrets/mega2-push-token}`，见 `config/config-storage-only.toml`）。该文件不进版本库，先创建：

```bash
mkdir -p secrets
openssl rand -hex 16 > secrets/mega2-push-token.local
```

> 若之后要跑仓库自带的 smoke 脚本，文件内容须改用 [`deploy-trunk.md`](./deploy-trunk.md) §8 登记的本地默认 token（该值以那里为唯一出处，本文不复制）。自生成的随机 token 对本文闭环同样有效。

启动并等待 healthcheck 通过（mega2 的就绪探针是 `/api/openapi.json`）：

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml up -d --wait
```

对象存储默认 RustFS（s3compatible），默认启动**不要**加 `--env-file`；只有改用本地文件系统后端时才需要，见 [`deploy-trunk.md`](./deploy-trunk.md) §8。

## 初始化（空卷 bootstrap）

首次启动后建库：创建 `main`、管理员与 [`monorepo.md`](./monorepo.md) 约定的顶层目录（`/project`、`/third-party` 等）：

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml exec -T mega2 \
  mega2 --config /etc/mega2/config.toml service init --yes
```

空卷上执行一次即可；`down -v` 重建后需重跑。

## 第一个闭环：clone → push → 读回

### 1. HTTP clone 子路径

storage-only 下对子路径 clone（不要对根 `/` 做根 clone，见 [`deploy-trunk.md`](./deploy-trunk.md) §9）。样例配置 `git.anonymous_access=true`，读无需凭据：

```bash
git clone http://127.0.0.1:9000/project
cd project
```

### 2. 用 push token 推送 main

monorepo 公开分支只有 `main`。推送走 HTTP Basic：username 任意，只有 password（token 密文）参与判定（[`deploy-trunk.md`](./deploy-trunk.md) §4）：

```bash
TOKEN=$(cat ../secrets/mega2-push-token.local)
echo "# hello mega2" > hello.md
git add hello.md && git commit -m "add hello.md"
git push "http://x:${TOKEN}@127.0.0.1:9000/project" main
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

## 停止与清理

```bash
# 停止，保留数据卷
docker compose -p mega2-trunk -f docker-compose-storage-only.yml down

# 连数据卷一起删除（破坏性；重跑需重新 bootstrap）
docker compose -p mega2-trunk -f docker-compose-storage-only.yml down -v
```

## 下一步

- 日常使用（clone / push / API / tag / LFS）：[`user-guide.zh.md`](./user-guide.zh.md)
- 配置键、Profile、SecretRef、热加载：[`configuration.zh.md`](./configuration.zh.md)；带注释的样例见 `config/config.toml`
- 部署与运维（token 管理、形态不变式、OCI、Agent Capture）：[`deployment.zh.md`](./deployment.zh.md)、[`deploy-trunk.md`](./deploy-trunk.md)
- 栈级协议 / API 写 smoke（git、LFS 黑盒）：[`deploy-trunk.md`](./deploy-trunk.md) §8.1
- 本地开发与测试：[`development.md`](./development.md)
