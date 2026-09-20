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

只能推送到 `main`（推送其它分支会被拒绝）；Git 客户端的 tag 推送同样被拒绝，tag 的创建 / 查看 / 删除走 HTTP API 或 `libra mega2 browser`（[`monorepo.md`](./monorepo.md)）。

### 3. API 读回与 Swagger UI

```bash
# 读回刚推送的文件内容，应输出 hello.md 的内容
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/hello.md"
```

完整 HTTP 表面（Git smart HTTP、LFS、产品写、tags、可选 OCI `/v2`）在 Swagger UI 浏览：`http://127.0.0.1:9000/swagger-ui`（OpenAPI JSON：`/api/openapi.json`）。交互式终端浏览用 Libra 的 `libra mega2 browser`；mega2 本身不提供 Web UI。

## 案例：推送嵌套目录（目录层级自动创建）

monorepo 里**不需要预先创建目录**。在 `/project` 的克隆里直接建多级目录再推送，`rust-lang/` 与 `rust-lang/crate/` 两层会随这次 push 一并写入：

```bash
git clone http://127.0.0.1:9000/project
cd project
mkdir -p rust-lang/crate
echo "# crate" > rust-lang/crate/README.md
git add . && git commit -m "add rust-lang/crate"
git push origin main
```

推送后，这个嵌套路径就是一个可独立 clone / push 的子路径，也可以直接经 API 读回：

```bash
# 读回嵌套路径下的文件
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/rust-lang/crate/README.md"

# 子路径克隆（不要对根 / 做根 clone）
git clone http://127.0.0.1:9000/project/rust-lang/crate
```

## 案例：推送一个已存在的 Git 仓库

迁入已有仓库（要保留分支与 tag）用 `/third-party` 下的 **ImportRepo**：那里按普通 Git 语义工作——多分支与客户端 tag 均合法（[`monorepo.md`](./monorepo.md)）。推送目标路径不存在时会在 push 中自动建仓，无需事先创建：

```bash
cd /path/to/your-repo
git remote add mega2 http://127.0.0.1:9000/third-party/your-repo
git push mega2 --all     # 推送全部分支
git push mega2 --tags    # 推送全部 tag
```

之后照常 clone / fetch：

```bash
git clone http://127.0.0.1:9000/third-party/your-repo
```

两种路径语义不要混淆：

- `/third-party/**`（ImportRepo）：多分支、客户端 tag 合法——适合托管第三方依赖源码、迁入已有仓库。
- 其它路径（Monorepo）：只有公开分支 `main`、Git 客户端禁 tag。已有仓库的多分支历史不能直接推进 monorepo 子路径；规则见 [`monorepo.md`](./monorepo.md)。

## 观察、停止与清理

```bash
# 跟随 mega2 日志
docker compose -f mega2-compose.yml logs -f mega2

# 停止，具名卷保留 Postgres / Redis / RustFS / mega2 数据
docker compose -f mega2-compose.yml down

# 连数据卷一起删除（破坏性，清空这个本机评估实例）
docker compose -f mega2-compose.yml down -v
```

## 重要：把数据持久化到本地目录

默认栈用的是 Docker 具名卷，`down -v` 会把数据连同卷一起删掉。如果你要**长期使用**这个实例（仓库数据需要跨重建保留），把数据库、对象存储与 mega2 数据目录改成绑定挂载到宿主本地目录——这样**即使容器和卷都被删除，数据仍保留在本地目录里，下次 `up` 直接继续使用**。

在仓库根新建一个覆盖文件 `mega2-compose.persist.yml`：

```yaml
# 与 mega2-compose.yml 叠加使用：把具名卷替换为宿主目录绑定挂载
services:
  postgres:                              # 元数据库
    volumes:
      - ./mega2-data/postgres:/var/lib/postgresql
  redis:                                 # 缓存 / 队列
    volumes:
      - ./mega2-data/redis:/data
  rustfs:                                # 对象存储（Git blob / LFS）
    volumes:
      - ./mega2-data/rustfs:/data
  mega2:                                 # mega2 数据目录（MEGA_BASE_DIR）
    volumes:
      - ./mega2-data/mega2:/var/lib/mega2
```

用双 `-f` 启动（停止、清理命令同样要带两个 `-f`）：

```bash
docker compose -f mega2-compose.yml -f mega2-compose.persist.yml up -d --wait
```

之后所有数据都落在 `./mega2-data/` 下：

- `docker compose ... down -v` 只删除具名卷，**不会**触碰 `./mega2-data/`；下次 `up` 从该目录恢复，仓库、推送历史、LFS 对象原样保留。
- 备份 = 停栈后打包 `./mega2-data/` 即可。
- 该目录由容器内进程写入（属主是容器内用户，如 postgres），不要提交进版本库，也不要手工改动目录内容。

## 其它 Compose 文件的定位

仓库里还有两份 Compose，**都面向测试与开发**，不是本评估栈的替代品：

- [`docker-compose-storage-only.yml`](../docker-compose-storage-only.yml)（及 `docker-compose-storage-only.auth-none.yml` 覆盖）：从源码**构建**镜像的 trunk 实验栈，token 鉴权、需要 `service init` bootstrap，用于部署演练与 smoke。用法见 [`deployment.zh.md`](./deployment.zh.md) 与 [`deploy-trunk.md`](./deploy-trunk.md) §8。
- [`docker-compose.test.yml`](../docker-compose.test.yml)：集成测试（IT）数据面，见 [`development.md`](./development.md)。

## 下一步

- 日常使用（clone / push / API / tag / LFS）：[`user-guide.zh.md`](./user-guide.zh.md)
- 配置键、Profile、SecretRef、热加载：[`configuration.zh.md`](./configuration.zh.md)；带注释的样例见 `config/config.toml`
- 部署与运维（token 管理、形态不变式、OCI、Agent Capture）：[`deployment.zh.md`](./deployment.zh.md)、[`deploy-trunk.md`](./deploy-trunk.md)
- 本地开发与测试：[`development.md`](./development.md)
