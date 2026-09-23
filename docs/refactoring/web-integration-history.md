# Website integration stack changes

[English](web-integration-history.en.md) · 中文

日期：2026-09-04  
范围：`docker/docker-compose.test.yml` web profile（服务名 `website-*` **保留**）

## 为何保留 `website-*` 命名

mega2 侧配置键与集成测试契约已冻结为 `MEGA_OAUTH__WEBSITE_*` /
`MEGA_NOTIFICATION__WEBSITE_*` 以及服务名 `website-next` / `website-db-init`
（见 README 联调栈命名契约与 ADR-WA-07）。DEP-01 只改**构建上下文与镜像内容**，
不改服务名 / 库名 `website` / 配置键，避免牵动 mega2 测试与配置面。

## 变更摘要

| 项 | 原值 | 新值 |
|---|---|---|
| `website-next` / `website-db-init` `build.context` | `../monoui` | 网站应用 sibling checkout |
| `dockerfile` | `apps/next-app/Dockerfile` | `apps/web/Dockerfile` |
| targets | （默认 runner）/ `builder` | `runner` / `db-init` |
| db-init 命令 | `drizzle-kit push --force` | `drizzle-kit migrate` |
| 新增服务 | — | `website-collab`（host `17002` → `7002`） |
| `website-next` 注入 | — | `MEGA_COLLAB_*` / `NEXT_PUBLIC_MEGA_COLLAB_PUBLIC_URL` |
| bucket | 仍默认 `monoui`（可选改名未做，见 ADR-ARCH-05） | 不变 |

## 原因

website frontend 取代 monoui 作为本联调栈的前端构建产物；collab 入仓后需独立
`website-collab` 服务供浏览器 WS 与内网 bridge 使用。
