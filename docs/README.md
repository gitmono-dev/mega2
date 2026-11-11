# Mega2 Documentation

English · [中文](README.zh.md)

The original Mega was the first-generation monorepo platform. Mega2 is the second-generation engine built for Agents, with two core capabilities: the Monorepo engine and Agent Traces archive. The recommended Agent setup combines Mega2, [ScorpioFS](https://github.com/gitmono-dev/scorpiofs), and [Libra](https://libra.tools): Mega2 hosts the Monorepo and exposes Traces archiving and retrieval APIs, ScorpioFS mounts repository paths as a local filesystem, and Libra provides Agent version-control workflows and terminal browsing.

The open-source edition of Mega2 has no Web UI. Run `libra mega2 browser --server <URL>` to open Libra's terminal browser: it browses remote directories one level at a time, supports creating, deleting, moving, and renaming directories, and can list, create, and delete root tags. Write credentials are required to create or delete tags. The command does not replace Git clone, fetch, or push.

Choose a guide by task. The quick start and user guide cover day-to-day use; configuration, deployment, and development guides are for operators and contributors. Subsystem contracts and planning records remain in their own directories.

## Mega2 core capabilities

| Capability | Description | Further reading |
|---|---|---|
| Monorepo engine | Hosts code trees and Git repositories. Use Git clients for clone, fetch, and push; use Libra browser for basic remote directory browsing and tag operations. | [User Guide](user-guide.md) |
| Agent Traces archive | Session history and operational trajectory archiving for AI agents. Uploaded via dedicated scoped Git references (`refs/libra/traces` over HTTP/SSH) and queried via read-only APIs (`/api/v1/traces/*`) with literal search. Features isolated credential binding and bounded protocol limits. Note: legacy `[agent_capture]` HTTP ingestion is retired. | [User Guide](user-guide.md#5-optional-service-surfaces) · [Traces protocol](refactoring/traces.md) · [Configuration Reference](configuration.md) |

Other capabilities—including LFS, OCI, build artifacts, Vault, notifications, and configuration—are covered in [Usage Recipes](recipes.md), the [User Guide](user-guide.md), and the [Configuration Reference](configuration.md).

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
| Deploy or harden a trunk / storage-only service | [Deployment Guide](deployment.md); operating rules are in the [trunk deployment manual](deploy-trunk.md) (Chinese) |
| Configure authorization or initialize the Monorepo directory layout | [Authentication and authorization manual](manual/authz.md) (Chinese) · [Architecture](architecture.md) · [Initialization Manual](manual/monorepo-init.md) |

## Development and contributions

| Task | Guide |
|---|---|
| Build locally, run tests, and start integration-test services | [Local development and testing](development.md) (Chinese) · [Contributing Guide](contributing.md) |
| Understand module boundaries, storage, and the write path | [Architecture](architecture.md) |
| Prepare a change, run the required checks, or add a module | [Contributing Guide](contributing.md); repository conventions are in [`../AGENTS.md`](../AGENTS.md) |
| Look up how error types map to HTTP status codes | [Error model](errors.md) (Chinese) · [Architecture](architecture.md) |

## Subsystem references and project records

- [`refactoring/`](refactoring/README.md) (Chinese): contracts and implementation records for subsystems such as configuration, protocols, storage, Vault, and notifications. Some documents record how a subsystem evolved; check the current source and config for shipped behavior.
- [`plan/`](plan/README.md) (Chinese): task cards, roadmaps, and plan templates. Plans record decisions and scheduled work, not the current implementation state.
- [`gap/`](gap/README.md) (Chinese): competitive gap analysis and follow-up leads.
- [OOM / SIGKILL troubleshooting notes](debug-oom-kill-monitoring.md) (Chinese): an internal troubleshooting record (not a user guide) for diagnosing and monitoring terminated test processes on the author's machine.
