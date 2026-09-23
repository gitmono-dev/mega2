# 快速开始

[English](quick-start.md) · 中文

在仓库根目录运行适用于当前平台的 Compose 文件，启动本地 mega2 评估栈：macOS + OrbStack 使用 [`macos-orbstack-mega2-compose.yml`](../macos-orbstack-mega2-compose.yml)，Linux 使用 [`linux-mega2-compose.yml`](../linux-mega2-compose.yml)。本指南会带你通过 HTTP 克隆仓库、推送到 `main`，再用 API 读回结果；后续示例还涵盖仓库迁移、Git LFS、OCI 镜像、构建产物和数据持久化。该栈从 **Docker Hub 拉取正式发布镜像**（`genedna/mega2:latest`），不从源码构建，也不需要单独执行初始化命令或提供 token。分支与 Tag 规则见[使用指南](./user-guide.zh.md)。

## 前置条件

- **macOS：**需要 OrbStack；macOS 版 Compose 文件依赖 `*.orb.local` DNS。
- **Linux：**需要 Docker Engine；Linux 版 Compose 文件让 mega2 使用 host networking。
- **两者都需要：**Docker Compose v2、`git` 和 `curl`。以下命令均从仓库根目录运行。
- 首次 `up` 会拉取 `genedna/mega2:latest`、PostgreSQL、Redis、RustFS 与 RustFS CLI 镜像，耗时取决于网络。

## 启动栈

```bash
# 按平台选择 compose 文件：
COMPOSE=macos-orbstack-mega2-compose.yml   # macOS + OrbStack
COMPOSE=linux-mega2-compose.yml            # Linux Docker

docker compose -f $COMPOSE up -d --wait
```

`up` 之后无需任何初始化命令：`rustfs-init` 自动创建 `mega2` 桶，mega2 在服务启动时自动初始化空的 Monorepo（`main` 与顶层目录）。就绪探针是 `/api/openapi.json`。

> **仅供本机使用：**该栈将 `push_auth` 设为 `none`，Git clone、fetch 和 push 均无需凭据，因此 9000 端口只绑定到 `127.0.0.1`。不要将其改为 `0.0.0.0`，也不要通过反向代理对外暴露。共享网络或公网部署应配置 token 鉴权，详见[部署指南](./deployment.zh.md)。

## 克隆、推送并验证

### 1. HTTP clone 子路径

在 storage-only 模式下，应克隆仓库子路径；不支持直接克隆根路径 `/`。

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

只能推送到 `main`，其它分支以及 Git 客户端发起的 Tag 推送都会被拒绝。请通过 HTTP API 或 Libra 命令 `libra mega2 browser` 创建、查看和删除 Tag；完整操作规则见[使用指南](./user-guide.zh.md)。

### 3. API 读回与 Swagger UI

```bash
# 读回刚推送的文件内容，应输出 hello.md 的内容
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/hello.md"
```

在 Swagger UI 中查看完整 HTTP API，包括 Git Smart HTTP、LFS、产品写入、Tag 和可选 OCI `/v2`：`http://127.0.0.1:9000/swagger-ui`。OpenAPI 文档位于 `/api/openapi.json`。mega2 不提供 Web UI；如需交互式浏览，请在 Libra 工作副本中运行 `libra mega2 browser`。

## 示例：推送嵌套目录

**无需预先创建目录。**在 `/project` 的克隆中直接建立多级目录并推送，`rust-lang/` 和 `rust-lang/crate/` 会随本次 push 一并写入：

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

## 示例：迁移现有 Git 仓库

如需迁入已有仓库并保留分支和 Tag，请将它放在 `/third-party` 下的 **ImportRepo** 路径中。该路径遵循常规 Git 语义，允许多个分支和由 Git 客户端管理的 Tag。目标路径不存在时，push 会自动创建仓库，无需预先设置：

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
- 其它路径（Monorepo）：公开分支只有 `main`，Git 客户端不能推送 Tag。已有仓库的多分支历史不能直接推入 Monorepo 子路径；详见[使用指南](./user-guide.zh.md)。

## 示例：镜像 GitHub 仓库（brewfs）

下面以 <https://github.com/brewfs/brewfs> 为例，演示如何通过一次 push 镜像已有 GitHub 仓库的全部分支和 Tag：

```bash
git clone --mirror https://github.com/brewfs/brewfs.git
cd brewfs.git
git lfs fetch --all origin                # 尽力而为；有一个历史对象在 GitHub 上已 404
git config lfs.allowincompletepush true   # 推送仍可用的 LFS 对象
git remote add mega2 http://127.0.0.1:9000/third-party/brewfs
git push --mirror mega2
```

两点说明：

- `--mirror` 会推送**所有** ref，包括全部分支和 Tag。相比之下，前一示例中的 `git push mega2 --all` 只推送本地分支；普通克隆通常只有 `main`。
- brewfs 使用 Git LFS 存储大型测试文件。其历史中引用的一个对象已从 GitHub 删除，因此 `git lfs fetch --all` 会报告 404，这是预期情况。将 `lfs.allowincompletepush` 设为 `true` 后，仍可推送当前可用的 LFS 对象。

验证带注释 Tag 和 LFS 文件都已完整往返：

```bash
git clone http://127.0.0.1:9000/third-party/brewfs /tmp/verify-brewfs
cd /tmp/verify-brewfs
git for-each-ref refs/tags                       # v0.0.1 / v0.1.1 / v0.1.2
git cat-file -t v0.1.2^{tag}                     # => tag（annotated tag 对象完好）
ls -la tests/scripts/xfstests-prebuilt/xfstests-prebuilt.tar.gz
# 约 8 MB 的 gzip——从 mega2 的 LFS 对象存储取回的是真实字节，不是指针
```

## 示例：使用 Git LFS 管理大文件

该栈在 `/info/lfs` 提供标准 Git LFS 协议，可直接使用常规 `git-lfs` 客户端。LFS 写入与 Git push 共用 `git.push_auth`；本评估栈允许匿名写入。请先在宿主机安装 `git-lfs`，并用 `git lfs version` 确认可用。

```bash
git clone http://127.0.0.1:9000/project
cd project
git lfs install --local                       # 每个克隆执行一次
mkdir -p lfs-demo && cd lfs-demo
git lfs track "*.bin"                         # 写入 .gitattributes
head -c 2097152 /dev/urandom > big.bin        # 一个 2 MiB 二进制
git add .gitattributes big.bin
git commit -m "add LFS-tracked big.bin"
git push origin main                          # 先执行 "Uploading LFS objects: 100% (1/1)"
```

Git 提交中保存的是 LFS 指针；实际的 2 MiB 文件内容会在 push 时上传到对象存储。再克隆该子路径，确认检出时取回的是原始文件：

```bash
git clone http://127.0.0.1:9000/project/lfs-demo /tmp/verify-lfs
cmp lfs-demo/big.bin /tmp/verify-lfs/big.bin && echo identical
```

LFS 对象和 Git blob 共用对象存储（本栈使用 RustFS），因此下文关于数据卷和持久化的说明同样适用。

## 示例：用 OCI 仓库托管容器镜像

该栈在同一端口的 `/v2` 路径提供 OCI Distribution 仓库，Compose 文件通过 `MEGA_OCI__ENABLED=true` 启用。OCI 仓库要求 storage-only 模式，本栈已通过 `MEGA_GIT__PUSH_AUTH` 配置满足此条件。任何 OCI 客户端都可连接；Docker 默认将 `127.0.0.1` 视为不安全仓库，因此本机评估可直接使用 HTTP：

```bash
docker pull alpine:3.21
docker tag alpine:3.21 127.0.0.1:9000/project/alpine:quickstart
docker push 127.0.0.1:9000/project/alpine:quickstart

# 回读验证：删掉本地 tag，从 mega2 拉取
docker rmi 127.0.0.1:9000/project/alpine:quickstart alpine:3.21
docker pull 127.0.0.1:9000/project/alpine:quickstart
docker run --rm 127.0.0.1:9000/project/alpine:quickstart cat /etc/alpine-release

# 或直接查询仓库 API
curl -s http://127.0.0.1:9000/v2/project/alpine/tags/list
```

仓库写入与 Git push 共用 `git.push_auth`；本评估栈允许匿名写入。Blob 和 manifest 都存放在 RustFS 对象存储中。若要从其它机器访问，请配置 TLS，或将仓库主机添加到相应 Docker 客户端的 `insecure-registries` 列表。

## 示例：用预签名 URL 上传和下载构建产物

将编译产物、发布包等文件作为 **Artifact Set** 存放在 `/api/v1/repos/{repo}/artifacts` 下。流程分为 discovery、batch、commit 三个请求。mega2 负责处理元数据并签发一小时后过期的 S3 预签名 URL；客户端通过该 URL 直接向对象存储（本例使用 RustFS）传输文件，不经过 mega2 中转。

```bash
REPO=project   # 单个 URL 路径段；名字里有 "/" 时用 %2F
BASE=http://127.0.0.1:9000/api/v1/repos/$REPO/artifacts

# 0. Discovery：协议版本、限额、支持的传输方式
curl -s $BASE/discovery

# 1. 准备两个发布文件；每个对象 id 是客户端生成的 UUID
head -c 1048576 /dev/urandom > app-1.0.0.tar.gz
shasum -a 256 app-1.0.0.tar.gz > SHA256SUMS
OID1=$(uuidgen | tr 'A-Z' 'a-z'); OID2=$(uuidgen | tr 'A-Z' 'a-z')

# 2. Batch：登记对象清单，为每个对象拿到预签名 PUT URL
curl -s -X POST -H 'Content-Type: application/json' $BASE/batch --data @- <<EOF
{"namespace": "releases", "object_type": "snapshot", "intent": "upload",
 "objects": [
   {"path": "app-1.0.0.tar.gz", "oid": "$OID1", "size": $(wc -c < app-1.0.0.tar.gz | tr -d ' '), "content_type": "application/gzip"},
   {"path": "SHA256SUMS", "oid": "$OID2", "size": $(wc -c < SHA256SUMS | tr -d ' '), "content_type": "text/plain"}],
 "metadata": {"run_id": "quickstart-demo"}}
EOF
# → {"transfer":"basic","objects":[{"oid":"...","exists":false,
#    "actions":{"upload":{"href":"http://<rustfs>/...?X-Amz-Signature=...",
#    "header":{"Content-Type":"..."},"expires_at":"..."}}}], ...}

# 3. 直接将文件上传到对象存储，不经过 mega2。
#    使用 actions.upload.header 返回的请求头：
curl -X PUT -H 'Content-Type: application/gzip' --data-binary @app-1.0.0.tar.gz "<OID1 的 href>"
curl -X PUT -H 'Content-Type: text/plain' --data-binary @SHA256SUMS "<OID2 的 href>"

# 4. Commit：把已上传的对象登记为一个 Artifact Set
curl -s -X POST -H 'Content-Type: application/json' $BASE/commit --data @- <<EOF
{"namespace": "releases", "object_type": "snapshot",
 "files": [
   {"path": "app-1.0.0.tar.gz", "oid": "$OID1", "size": $(wc -c < app-1.0.0.tar.gz | tr -d ' ')},
   {"path": "SHA256SUMS", "oid": "$OID2", "size": $(wc -c < SHA256SUMS | tr -d ' ')}],
 "metadata": {"run_id": "quickstart-demo"}}
EOF
# → {"artifact_set_id":"...","status":"ok","missing_objects":[]}
```

确认 `missing_objects` 为空；其中列出的对象未能在存储中找到，因此不会加入已提交的 Artifact Set。要读回文件，请使用预签名下载 URL；`curl -L` 会跟随重定向并连接到 RustFS：

```bash
# 列出集合 / 把文件解析为对象 id
curl -s "$BASE/sets?namespace=releases&object_type=snapshot"
curl -s "$BASE/resolve-file?namespace=releases&object_type=snapshot&path=app-1.0.0.tar.gz"

# 下载：302 跳转到预签名 GET，或返回 JSON 链接（?mode=link）
curl -L -o dl.tar.gz "$BASE/objects/$OID1"
cmp app-1.0.0.tar.gz dl.tar.gz && echo identical
curl -s "$BASE/objects/$OID2?mode=link"
```

产物写入与 `git.push_auth` 共用同一道闸门（本栈为匿名）。`object_type` 词汇表（`snapshot`、`provenance`、`run` 等）与协商限额都由 discovery 响应公布。

## 查看日志、停止和清理

```bash
# 跟随 mega2 日志
docker compose -f $COMPOSE logs -f mega2

# 停止，具名卷保留 Postgres / Redis / RustFS / mega2 数据
docker compose -f $COMPOSE down

# 连数据卷一起删除（破坏性，清空这个本机评估实例）
docker compose -f $COMPOSE down -v
```

## 示例：将数据保存到宿主机目录

默认栈使用 Docker 具名卷，执行 `down -v` 会删除这些卷及其中的数据。如果要在移除栈后保留数据，请将数据库、对象存储和 mega2 数据目录绑定挂载到宿主机目录。这样即使容器和具名卷被删除，数据仍会留在宿主机上，下一次 `up` 会继续使用这些数据。

在仓库根新建一个覆盖文件 `mega2-compose.persist.yml`：

```yaml
# 与你选用的 compose 文件叠加使用：把具名卷替换为宿主目录绑定挂载
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
docker compose -f $COMPOSE -f mega2-compose.persist.yml up -d --wait
```

之后所有数据都落在 `./mega2-data/` 下：

- `docker compose ... down -v` 只删除具名卷，**不会删除** `./mega2-data/`。下次启动会从该目录继续使用原有仓库、推送历史和 LFS 对象。
- 备份前先停止服务，再归档 `./mega2-data/`。
- 目录由容器内进程写入，文件属主可能是容器中的用户（例如 `postgres`）。不要将其提交到版本库，也不要手工修改目录内容。

## 区分评估栈与测试栈

仓库还提供另外两份 Compose 文件，**用途都是测试和开发**，不能直接替代本评估栈：

- [`docker/docker-compose-storage-only.yml`](../docker/docker-compose-storage-only.yml)（及 `docker/docker-compose-storage-only.auth-none.yml` 覆盖）：从源码**构建**镜像的 trunk 测试栈，使用 token 鉴权，并需先运行 `service init`；适合部署演练和 smoke 测试。用法见[部署指南](./deployment.zh.md)和[`deploy-trunk.md`](./deploy-trunk.md)第 8 节。
- [`docker/docker-compose.test.yml`](../docker/docker-compose.test.yml)：集成测试（IT）数据面，见 [`development.md`](./development.md)。

## 下一步

- 日常使用（clone / push / API / tag / LFS）：[`user-guide.zh.md`](./user-guide.zh.md)
- 配置键、Profile、SecretRef、热加载：[`configuration.zh.md`](./configuration.zh.md)；带注释的样例见 `config/config.toml`
- 部署与运维（token 管理、形态不变式、OCI、Agent Capture）：[`deployment.zh.md`](./deployment.zh.md)、[`deploy-trunk.md`](./deploy-trunk.md)
- 本地开发与测试：[`development.md`](./development.md)
