# 快速开始

[English](quick-start.md) · 中文

本文用仓库根对应平台的 compose 文件——macOS + OrbStack 用 [`macos-orbstack-mega2-compose.yml`](../macos-orbstack-mega2-compose.yml)，Linux Docker 用 [`linux-mega2-compose.yml`](../linux-mega2-compose.yml)——在本机拉起 mega2（trunk / storage-only）评估栈，跑通第一个闭环：HTTP clone → push `main` → API 读回。该栈直接从 **Docker Hub 拉取正式发布镜像**（`genedna/mega2:latest`），不构建源码、不需要 bootstrap、不需要 token。产品规则见 [`monorepo.md`](./monorepo.md)。

## 前置条件

- macOS：需要 OrbStack（macOS 版 compose 依赖其 `*.orb.local` DNS）。Linux：Docker Engine，Linux 版 compose 对 mega2 使用 host networking。两者都需要 Compose plugin（v2）、`git`、`curl`；全程在仓库根目录执行。
- 首次 `up` 会拉取 `genedna/mega2:latest`、PostgreSQL、Redis、RustFS 与 RustFS CLI 镜像，耗时取决于网络。

## 启动栈

```bash
# 按平台选择 compose 文件：
COMPOSE=macos-orbstack-mega2-compose.yml   # macOS + OrbStack
COMPOSE=linux-mega2-compose.yml            # Linux Docker

docker compose -f $COMPOSE up -d --wait
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

## 案例：镜像一个 GitHub 仓库（brewfs）

完整镜像一个已有 GitHub 仓库——全部分支与 tag——只需一次推送。以 <https://github.com/brewfs/brewfs> 为例：

```bash
git clone --mirror https://github.com/brewfs/brewfs.git
cd brewfs.git
git lfs fetch --all origin                # 尽力而为；有一个历史对象在 GitHub 上已 404
git config lfs.allowincompletepush true   # 推送仍可用的 LFS 对象
git remote add mega2 http://127.0.0.1:9000/third-party/brewfs
git push --mirror mega2
```

两点说明：

- `--mirror` 推送**全部** ref（所有分支 + 所有 tag）。上一案例的 `git push mega2 --all` 只推*本地*分支，普通克隆里通常只有 `main`。
- brewfs 用 Git LFS 管理大型测试固件。旧历史引用的一个对象在 GitHub 上已不存在，所以 `git lfs fetch --all` 会打印 404 错误——属预期；`lfs.allowincompletepush true` 允许推送在缺少该对象的情况下继续。

验证回路——annotated tag 与 LFS 内容都完好：

```bash
git clone http://127.0.0.1:9000/third-party/brewfs /tmp/verify-brewfs
cd /tmp/verify-brewfs
git for-each-ref refs/tags                       # v0.0.1 / v0.1.1 / v0.1.2
git cat-file -t v0.1.2^{tag}                     # => tag（annotated tag 对象完好）
ls -la tests/scripts/xfstests-prebuilt/xfstests-prebuilt.tar.gz
# 约 8 MB 的 gzip——从 mega2 的 LFS 对象存储取回的是真实字节，不是指针
```

## 案例：用 Git LFS 管理大文件

该栈讲标准 Git LFS 协议（`/info/lfs`），stock `git-lfs` 客户端开箱即用。LFS 写授权与 Git push 共用 `git.push_auth`——本评估栈为匿名。前置条件：宿主机已安装 `git-lfs`（用 `git lfs version` 确认）。

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

Git 提交里只存 LFS 指针；2 MiB 字节在 push 时已进入对象存储。从子路径全新克隆验证——检出时会下载真实内容：

```bash
git clone http://127.0.0.1:9000/project/lfs-demo /tmp/verify-lfs
cmp lfs-demo/big.bin /tmp/verify-lfs/big.bin && echo identical
```

LFS 对象与 Git blob 共用同一套对象存储（本栈为 RustFS），下文关于数据卷、`down -v` 与持久化覆盖文件的说明对它同样适用。

## 案例：用 OCI 仓库托管容器镜像（/v2）

该栈在同一端口的 `/v2` 下内嵌了一个 OCI Distribution 仓库（Compose 文件中通过 `MEGA_OCI__ENABLED=true` 启用；它还要求 storage-only 形态，`MEGA_GIT__PUSH_AUTH` 已隐含满足）。任意 OCI 客户端均可使用——Docker 默认把 `127.0.0.1` 当作 insecure 仓库，所以本地评估用纯 HTTP 即可：

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

仓库写入与 `git.push_auth` 共用同一道闸门——本评估栈为匿名。blob 与 manifest 与其它数据一样落在 RustFS 对象存储里。若要把仓库暴露给其它机器，默认的 Docker 客户端不再接受纯 HTTP：请在前面加 TLS，或把该主机加入客户端的 `insecure-registries`。

## 案例：用预签名直连管理构建产物

编译产物、发布包等二进制以 **Artifact Set** 的形式存放在 `/api/v1/repos/{repo}/artifacts` 下。流程是 discovery → batch → commit 三步，字节传输走**预签名（presigned）**：mega2 只处理元数据并签发限时（1 小时）的 S3 URL——客户端直接对对象存储（这里是 RustFS）上传/下载字节，不经过 mega2 中转。

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

# 3. 把字节直接上传到对象存储（不是 mega2）。
#    请求头按 actions.upload.header 返回的来带：
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

`missing_objects` 必须为空——出现在其中的对象说明对象存储里没找到，commit 只登记了其余对象。回读验证；下载同样走预签名，`curl -L` 会跟随 302 直连 RustFS：

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

## 观察、停止与清理

```bash
# 跟随 mega2 日志
docker compose -f $COMPOSE logs -f mega2

# 停止，具名卷保留 Postgres / Redis / RustFS / mega2 数据
docker compose -f $COMPOSE down

# 连数据卷一起删除（破坏性，清空这个本机评估实例）
docker compose -f $COMPOSE down -v
```

## 重要：把数据持久化到本地目录

默认栈用的是 Docker 具名卷，`down -v` 会把数据连同卷一起删掉。如果你要**长期使用**这个实例、希望删除栈之后数据还在，就把数据库、对象存储与 mega2 数据目录改成绑定挂载到宿主本地目录——这样**即使容器和卷都被删除，数据仍保留在本地目录里，下次 `up` 直接继续使用**。

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

- `docker compose ... down -v` 只删除具名卷，**不会**触碰 `./mega2-data/`；下次 `up` 从该目录恢复，仓库、推送历史、LFS 对象原样保留。
- 备份 = 停栈后打包 `./mega2-data/` 即可。
- 该目录由容器内进程写入（属主是容器内用户，如 postgres），不要提交进版本库，也不要手工改动目录内容。

## 其它 Compose 文件的定位

仓库里还有两份 Compose，**都面向测试与开发**，不是本评估栈的替代品：

- [`docker/docker-compose-storage-only.yml`](../docker/docker-compose-storage-only.yml)（及 `docker/docker-compose-storage-only.auth-none.yml` 覆盖）：从源码**构建**镜像的 trunk 实验栈，token 鉴权、需要 `service init` bootstrap，用于部署演练与 smoke。用法见 [`deployment.zh.md`](./deployment.zh.md) 与 [`deploy-trunk.md`](./deploy-trunk.md) §8。
- [`docker/docker-compose.test.yml`](../docker/docker-compose.test.yml)：集成测试（IT）数据面，见 [`development.md`](./development.md)。

## 下一步

- 日常使用（clone / push / API / tag / LFS）：[`user-guide.zh.md`](./user-guide.zh.md)
- 配置键、Profile、SecretRef、热加载：[`configuration.zh.md`](./configuration.zh.md)；带注释的样例见 `config/config.toml`
- 部署与运维（token 管理、形态不变式、OCI、Agent Capture）：[`deployment.zh.md`](./deployment.zh.md)、[`deploy-trunk.md`](./deploy-trunk.md)
- 本地开发与测试：[`development.md`](./development.md)
