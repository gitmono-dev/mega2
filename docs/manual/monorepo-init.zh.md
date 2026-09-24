[English](monorepo-init.md) · 中文

# Monorepo 初始化与目录结构设置手册

本文面向运维与开发者，说明 mega2 在 **Monorepo 初始化**时如何生成目录结构、如何通过配置定制，以及初始化后的实际产物。产品规则（单公开分支、tag 限制、ImportRepo 例外）以 [使用指南](../user-guide.zh.md) 为准，本文不重复其规则细节。

> **范围**：默认路径下的 Monorepo（根路径 `/` 及非 `import_dir` 子树）的首次初始化。
> **不适用**：`import_dir` 下的 ImportRepo；已初始化库的目录重建（不支持，见下文）。

---

## 1. 初始化时机与入口

- 每次服务启动时，`Context::new` 调用 `MonoService::init_monorepo`（`src/context/mod.rs` → `src/jupiter/service/mono_service.rs::init_monorepo`）。
- **幂等**：若根路径 `/` 已存在 `main` ref，直接跳过（日志输出 `Monorepo Directory Already Inited`）。因此目录结构只在**全新数据库首次启动**时生成一次。
- **事务性**：初始 commit、`main` ref、tree/blob 元数据在同一 Postgres 事务内批量写入；blob 原始内容经 `GitService::put_objects` 写入对象存储。

## 2. 配置输入（`[monorepo]`）

初始化布局完全由配置驱动，字段定义见 `src/config/model.rs::MonoConfig`，样例见 `config/config.toml`：

| 字段 | 作用 | 对目录结构的影响 |
|---|---|---|
| `root_dirs` | 根树下一层目录名列表 | 每个名字生成一个一级目录，内含独立 `.gitkeep` 占位 |
| `import_dir` | ImportRepo 根（默认 `/third-party`） | 不改变树的生成，只决定其下路径按 ImportRepo 语义工作（多分支、客户端 tag 合法）；首组件必须列在 `root_dirs` 中（校验强制） |
| `admin` | 系统管理员列表 | 写入根级 `.mega_cedar.json` 实体，并成为 `.cedar/policies.cedar` 的全库默认 reviewer |
| `rename.*` | diff 重命名检测参数 | 与初始化布局无关 |

约束与生效方式：

- `import_dir` / `root_dirs` / `admin` 均校验非空；`root_dirs` 与 `import_dir` 另有形状校验（`src/config/validate.rs`，规则见 [`../configuration.zh.md`](../configuration.zh.md) 的 `[monorepo]` 条目）：`root_dirs` 每项为唯一的单组件目录名且不占用根树保留项，`import_dir` 为规范绝对非根路径且首组件列在 `root_dirs` 中。`config validate`、服务启动与热重载候选同一校验，配置错误会导致启动失败并点名字段。
- 这些字段属于热加载的 `restart_required_fields`：修改后必须**重启进程**。
- 配置变更只影响**尚未初始化**的新库；已初始化库不会随配置变更自动重建树。
- **目录层级限制**：`root_dirs` 只支持**一级目录名**，含 `/` 的条目在校验阶段即被拒绝（详见第 5 节「目录层级限制」）。

## 3. 目录结构的生成规则

生成逻辑在 `src/jupiter/utils/converter.rs`（`MegaModelConverter::init` → `init_trees`），按以下顺序拼装根树：

1. **`root_dirs` → 一级目录**：每个目录生成一个 `.gitkeep` blob，内容为 `Placeholder file for /<dir> directory`——每个目录内容不同是有意设计，保证各目录 tree hash 互不相同。
2. **`.mega_cedar.json`**（根级文件）：由 `admin` 列表经 `generate_entity` 生成的 Cedar 策略实体。
3. **`.cedar/policies.cedar`**（目录）：`admin` 作为全库默认 reviewer 的 Cedar 策略；`admin` 为空时内容为空。
4. **`.buckroot` + `.buckconfig`**（根级文件）：Buck 构建占位。`.buckroot` 为空；`.buckconfig` 写入 cells 定义（`root` / `prelude` / `toolchains` / `buckal` / `none`）与 cell 别名。
5. **`toolchains/BUCK`**：仅当 `root_dirs` 含 `toolchains` 时额外注入，内容为 `system_demo_toolchains()` 的 demo 工具链定义。
6. **排序与提交**：根树条目按 Git tree 规则排序（`sort_git_tree_items`，点开头文件排在目录项之前），生成 root tree，再创建无父初始 commit（message：`Init Mega Directory`）与 `refs/heads/main` ref。

初始化**不会**创建任何 `refs/tags/*`，也**不会**创建第二个 `refs/heads/*`（见 `../user-guide.zh.md` 的分支/tag 规则）。

## 4. 建议的初始化目录结构

建议把 `root_dirs` 设为以下八个一级目录（当前 `config/config.toml` 已采用）：

```toml
root_dirs = ["third-party", "project", "doc", "artifact", "release", "model", "data", "toolchains"]
```

### 各目录的作用

先说边界：引擎只对 **`import_dir` 对齐**（校验强制 `import_dir` 首组件列在 `root_dirs` 中）与 **`toolchains` 的 BUCK 注入**做特殊处理；其余目录的用途是**产品约定**，引擎不强制——初始化时对每个目录一律只写入 `.gitkeep` 占位，目录的真正语义由团队使用习惯承载。

| 目录 | 作用 |
|---|---|
| `third-party/` | 第三方导入仓库的归放根，**与 `import_dir` 对齐**（默认配置；校验要求 `import_dir` 的首组件列在 `root_dirs` 中）；其下每个子路径是一个 ImportRepo，按普通 Git 语义工作（多分支、客户端 tag 合法），Monorepo 的单分支/禁客户端 tag 规则不适用。详见下文「`import_dir` 的作用」 |
| `project/` | 业务工程源码主目录：团队的日常开发代码按子项目组织于此 |
| `doc/` | 文档目录：设计文档、规范、评审材料等与代码同仓管理的文档 |
| `artifact/` | 构建/打包产物的归放约定（如发布包、产物清单）。注意引擎另有独立的 artifact 存储子系统（buck/artifacts API 与 `artifact_objects` 表，经对象存储承载），与本目录的约定不是同一物 |
| `release/` | 发布物与发布记录：版本快照、发布说明等按版本组织 |
| `model/` | 模型类资产的归放约定（ML 模型、数据模型等）；体积大的文件应走 Git LFS |
| `data/` | 数据类资产的归放约定（数据集、共享样例/参考数据）；同样建议大文件走 LFS |
| `toolchains/` | Buck2 工具链定义目录。初始化时除 `.gitkeep` 外额外注入 `BUCK`（`system_demo_toolchains()`），根级 `.buckconfig` 的 cells 也把 `toolchains` 指向本目录 |

### 初始化后的实际逻辑根

```text
/                            refs/heads/main → 初始 commit "Init Mega Directory"
├── .buckconfig              # Buck cells 定义
├── .buckroot                # 空文件
├── .cedar/
│   └── policies.cedar       # admin = 全库默认 reviewer
├── .mega_cedar.json         # Cedar 实体（admin 列表）
├── third-party/             # = import_dir，其下为 ImportRepo
│   └── .gitkeep
├── project/
│   └── .gitkeep
├── doc/
│   └── .gitkeep
├── artifact/
│   └── .gitkeep
├── release/
│   └── .gitkeep
├── model/
│   └── .gitkeep
├── data/
│   └── .gitkeep
└── toolchains/
    ├── .gitkeep
    └── BUCK                 # system_demo_toolchains()
```

### `import_dir` 的作用

`import_dir`（默认 `/third-party`）不是普通的一级目录声明，而是 **ImportRepo 与 Monorepo 的路径边界**：

- **路径分类**：协议层按请求路径是否落在 `import_dir` 之下，把仓库判定为 ImportRepo 或 Monorepo（`GitProtocolPath`，见 [`../refactoring/protocol.md`](../refactoring/protocol.md)）。分类按路径前缀判定，与该目录是否已存在于树中无关。
- **规则豁免**：`import_dir` 下的 ImportRepo 允许普通 Git 多分支语义、允许客户端推送/删除 tag；Monorepo 区域则只有公开 `main` 分支、tag 仅经 Web/API、非 `main` 推送进入 `refs/cl/*` 管线（规则全文见 [使用指南](../user-guide.zh.md)）。
- **不参与树的生成**：`import_dir` 本身不产生目录——目录来自 `root_dirs`。因此校验要求 `import_dir` 的首组件必须列在 `root_dirs` 中（例如 `root_dirs = ["project"]` 配 `import_dir = "/vendor"` 会被拒绝），保证 ImportRepo 首次挂载不会新建顶层目录。
- **import 优先**：`import_dir` 之下的路径一律按 ImportRepo 处理（由 Git 推送创建），即使 `root_dirs` 含同名项。默认 `root_dirs` 含 `third-party` 且 `import_dir = "/third-party"`。修改 `import_dir` 属 `restart_required`，且只影响路径分类，不影响已生成的树。

注意默认值差异：`config init` 生成的模板（`src/config/template.rs`）与 `MonoConfig::default`（`src/config/model.rs`）的 `root_dirs` 都不含 `artifact` / `data`（模板也不含 `model`）。如需本文建议的八目录布局，请在首次初始化前于 `config.toml` 显式设置 `root_dirs`；**实际布局以运行配置为准**。

## 5. 如何定制目录结构

1. **在首次启动前**编辑 `config/config.toml` 的 `[monorepo].root_dirs`，增删一级目录名。
2. 若把 `third-party` 改名或移除，必须同步调整 `import_dir`，使其首组件仍列在 `root_dirs` 中，否则 `config validate` 与启动都会失败。
3. 设置 `admin` 为真实的系统管理员账号列表（写入 Cedar 实体与默认 reviewer）。
4. 用 `cargo run -p mega2 -- --config config/config.toml config validate` 预检配置合法性。
5. 以**空数据库**（或未初始化过的库）启动服务，初始化自动完成；可用 `git ls-remote` 或 clone 后 `git ls-tree` 核对根树与本文第 4 节一致。

### 目录层级限制：只初始化一级目录

`init_trees` 把 `root_dirs` 的每个字符串**原样**作为根树中单个条目的名字（`src/jupiter/utils/converter.rs`），不按 `/` 分段递归建树。因此配置校验（`src/config/validate.rs`）要求每项是单组件名：

- `root_dirs = ["a/b"]`、含 `\`、NUL、首尾空格，或为 `.`、`..`、空串的条目**在校验阶段即被拒绝**，不会生成含非法条目名的根树；
- 与 `init_trees` 自带根条目同名（`.cedar`、`.mega_cedar.json`、`.buckroot`、`.buckconfig`）或为 `.git`（不区分大小写）的条目同样被拒绝，避免根树重复条目或客户端拒绝 checkout；
- `toolchains` 的 BUCK 注入按条目名 `toolchains` 精确匹配。

**多级目录的正确做法**：初始化只铺设一级骨架；更深层级由客户端正常提交产生——clone 后 `mkdir -p a/b && git add && git commit && git push`（非 `main` 推送进入 CL 管线，合并后落地 `main`）。

已初始化库要变更布局：不支持自动重建；只能由客户端正常提交目录变更，或在明确接受数据重置的前提下清空数据面后重新初始化（属破坏性操作，本文不提供步骤）。

## 6. 容器化部署：挂载配置文件（生产方案）

### 配置定位与叠加顺序

服务按以下顺序定位配置文件，命中即停（`src/config/loader.rs`）：

1. CLI `--config <path>`；
2. 环境变量 `MEGA_CONFIG`；
3. 当前目录 `./config/config.toml`；
4. `{MEGA_BASE_DIR}/etc/config.toml`；
5. 均不存在时自动生成默认配置到第 4 条路径。

加载时按 **base 文件 → profile 文件（`--profile` / `MEGA_PROFILE` 对应同目录 `config.<name>.toml`）→ 环境变量** 的顺序叠加，后者覆盖前者（`src/config/source.rs`）。env 映射规则：`MEGA_` 前缀 + `__` 分层（如 `MEGA_DATABASE__DB_URL` → `[database] db_url`）；仅 `oauth.allowed_cors_origins`、`oauth.session_cookie_names`、`monorepo.admin`、`monorepo.root_dirs` 四个列表键支持逗号分隔。注意：`MEGA_MAIL__*` 一律被拒绝（本仓邮件已退场），配置文件含未知键会被白名单校验拒绝。

### 镜像基线

官方 `Dockerfile` 已把样例配置烘焙进镜像并预设好指向：

```dockerfile
COPY mega2/config/config.toml /etc/mega2/config.toml
ENV MEGA_BASE_DIR=/var/lib/mega2 MEGA_CONFIG=/etc/mega2/config.toml
ENTRYPOINT ["/usr/local/bin/mega2"]
CMD ["--config", "/etc/mega2/config.toml", "service", "http", "--host", "0.0.0.0", "-p", "8000"]
```

烘焙适合样例与 IT；**生产建议用挂载覆盖同一路径**——`MEGA_CONFIG` 与 CMD 均已指向它，挂载后无需改启动命令。

### 生产挂载

**Docker / docker-compose：**

```bash
docker run -d --name mega2 \
  -v /srv/mega2/config.toml:/etc/mega2/config.toml:ro \
  -v mega2-data:/var/lib/mega2 \
  -e MEGA_DATABASE__DB_URL='postgres://user:***@postgres:5432/mega2' \
  -e MEGA_REDIS__URL='redis://redis:6379' \
  -p 8000:8000 mega2:local
```

要点：配置文件**只读挂载**（`:ro`）；环境相关项（数据面端点、凭据）不写入挂入的文件，改由 env 注入；`MEGA_BASE_DIR` 对应的数据目录挂持久卷。本仓 `docker/docker-compose.test.yml` 的 `mega2` 服务即「镜像基线 + env 覆盖」的完整参照。

**Kubernetes（ConfigMap + Secret）：**

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: mega2-config
data:
  config.toml: |
    base_dir = "/var/lib/mega2"
    [monorepo]
    import_dir = "/third-party"
    admin = ["admin"]
    root_dirs = ["third-party", "project", "doc", "artifact", "release", "model", "data", "toolchains"]
---
# Pod 片段：
volumeMounts:
  - name: config
    mountPath: /etc/mega2/config.toml
    subPath: config.toml
    readOnly: true
env:
  - name: MEGA_DATABASE__DB_URL
    valueFrom: { secretKeyRef: { name: mega2-data-plane, key: database-url } }
  - name: MEGA_REDIS__URL
    valueFrom: { secretKeyRef: { name: mega2-data-plane, key: redis-url } }
volumes:
  - name: config
    configMap: { name: mega2-config }
```

**Secret 边界：** 对象存储、Redis 等凭据可在配置文件中写 `vault://` SecretRef（`src/config/secret.rs`），运行时经 Vault 解析、明文不落盘；但**数据库凭据不能使用 vault SecretRef**（引导循环硬约束），只能经 env/文件注入。不要把任何明文生产凭据烘焙进镜像或提交进仓库。

### 挂载后的校验与生效

- 部署前预检（ENTRYPOINT 即二进制，直接跟子命令）：

  ```bash
  docker run --rm -v /srv/mega2/config.toml:/etc/mega2/config.toml:ro \
    mega2:local config validate --deny-warnings
  ```

- 排错时加 `--show-sources` 查看每个键来自文件还是 env。
- `[monorepo]` 的 `root_dirs` / `import_dir` / `admin` 属 `restart_required_fields`，且只在**首次初始化**时生效——挂载的配置必须在**首次启动前定稿**（见第 2、5 节）；之后的改动需重建容器，且不会重建已初始化的目录树。

## 7. 物理存储

逻辑树不落在磁盘 bare 仓库：

- **Postgres**：commit / tree / blob 元数据与 refs（`mega_*` 表）。
- **对象存储**（orbit，local FS / S3 / GCS）：blob 内容字节。

运维路径由 `base_dir`、数据库与对象存储配置决定，见 [`../refactoring/config.md`](../refactoring/config.md) 与 [`../refactoring/orbit.md`](../refactoring/orbit.md)。

## 8. 交叉引用

| 主题 | 位置 |
|---|---|
| Monorepo 产品规则（分支 / tag / ImportRepo） | [使用指南](../user-guide.zh.md) |
| 协议路径判定与 smoke 矩阵 | [`../refactoring/protocol.md`](../refactoring/protocol.md)、`scripts/git_protocol_smoke.sh` |
| 初始化实现 | `src/jupiter/service/mono_service.rs::init_monorepo`、`src/jupiter/utils/converter.rs`（`init_trees` / `MegaModelConverter::init`） |
| 配置模型与默认值 | `src/config/model.rs::MonoConfig`、`config/config.toml` `[monorepo]` |
| 配置加载链与容器基线 | `src/config/loader.rs`、`src/config/source.rs`、`Dockerfile`、`docker/docker-compose.test.yml` |
| 结构排序回归测试 | `src/jupiter/utils/converter.rs` `init_trees_sorts_git_tree_entries` |
| 本地开发入口 | [`../development.md`](../development.md) |
