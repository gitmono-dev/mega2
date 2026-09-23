# Documentation

English · [中文](README.zh.md)

Choose a guide by task. The quick start and user guide cover day-to-day use; configuration, deployment, and development guides are for operators and contributors. mega2 does not provide a Web UI; use `libra mega2 browser` from a Libra working copy for interactive repository browsing and supported directory or tag operations. Subsystem contracts and planning records remain in their own directories.

## Get started

| Task | Guide |
|---|---|
| Start mega2 locally and make your first push | [Quick Start](quick-start.md) |
| Follow examples for repository migration, LFS, OCI, artifacts, and persistent data | [Usage Recipes](recipes.md) |
| Learn Git, HTTP API, Libra, and CLI workflows | [User Guide](user-guide.md) |

## Configuration and deployment

| Task | Guide |
|---|---|
| Find config sources, SecretRef, hot reload, and validation behavior | [Configuration Reference](configuration.md); full settings are in [`../config/config.toml`](../config/config.toml) |
| Deploy or harden a trunk / storage-only service | [Deployment Guide](deployment.md) |
| Review authorization behavior or initialize the Monorepo directory layout | [Architecture Guide](architecture.md) · [Initialization Manual](manual/monorepo-init.md) |

## Development and contributions

| Task | Guide |
|---|---|
| Build locally, run tests, and start integration-test services | [Contributing Guide](contributing.md) |
| Understand module boundaries, storage, and the write path | [Architecture](architecture.md) |
| Prepare a change, run the required checks, or add a module | [Contributing Guide](contributing.md); repository conventions are in [`../AGENTS.md`](../AGENTS.md) |
| Review error handling and service boundaries | [Architecture](architecture.md) |

## Subsystem references and project records

- Subsystem implementation notes and planning records are kept under `refactoring/` and `plan/`; check the current source and config for shipped behavior.
- Product-gap analysis and follow-up records are archived under `gap/`.
- [OOM / SIGKILL troubleshooting notes](debug-oom-kill-monitoring.md): diagnosing and monitoring terminated test processes on the author's machine.
