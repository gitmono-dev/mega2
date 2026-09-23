# 文档导航

[English](README.md) · 中文

按你的任务从这里开始。快速开始和使用指南面向使用者；配置、部署和开发文档面向运维人员与贡献者。mega2 不提供 Web UI；需要互动式浏览仓库或执行受支持的目录、Tag 操作时，请在 Libra 工作副本中运行 `libra mega2 browser`。子系统契约和计划记录保留在各自目录中。

## 开始使用

| 任务 | 文档 |
|---|---|
| 在本机启动 mega2 并完成首次推送 | [快速开始](quick-start.zh.md) |
| 查看仓库迁移、LFS、OCI、构建产物和数据持久化示例 | [进阶使用场景](recipes.zh.md) |
| 了解 Git、HTTP API、Libra 和 CLI 操作 | [使用指南](user-guide.zh.md) |

## 配置与部署

| 任务 | 文档 |
|---|---|
| 查找配置来源、SecretRef、热加载和校验方式 | [配置参考](configuration.zh.md)；完整配置项见 [`../config/config.toml`](../config/config.toml) |
| 部署或加固 trunk / storage-only 服务 | [部署指南](deployment.zh.md)；运行规则见 [trunk 部署手册](deploy-trunk.md) |
| 配置授权或初始化 Monorepo 目录 | [认证与授权手册](manual/authz.md) · [初始化手册](manual/monorepo-init.zh.md) |

## 开发与贡献

| 任务 | 文档 |
|---|---|
| 本地构建、运行测试和启动集成测试服务 | [本地开发与测试](development.md) |
| 了解模块边界、数据存储和写入流程 | [架构设计](architecture.zh.md) |
| 准备代码改动、运行提交门禁和新增模块 | [贡献指南](contributing.zh.md)；仓库约定见 [`../AGENTS.md`](../AGENTS.md) |
| 查阅错误类型和 HTTP 状态码的对应规则 | [错误模型](errors.md) |

## 子系统与历史记录

- [`refactoring/README.md`](refactoring/README.md)：配置、协议、存储、Vault、通知等子系统的契约和实现记录。部分文档记录了演进过程；涉及当前行为时，请以源码和配置为准。
- [`plan/`](plan/README.md)：任务卡、路线图和计划模板。计划记录的是决策与工作安排，不代表当前实现状态。
- [`gap/`](gap/README.md)：竞品差距分析与待办线索。
- [OOM / SIGKILL 排查记录](debug-oom-kill-monitoring.md)：本机测试进程被终止时的诊断与监控方法。
