# Documentation

English · [中文](README.zh.md)

The original Mega was the first-generation monorepo platform. Mega2 is the second-generation engine built for Agents, with two core capabilities: the Monorepo engine and optional Agent Session Capture. The recommended Agent setup combines Mega2, [ScorpioFS](https://github.com/gitmono-dev/scorpiofs), and [Libra](https://libra.tools): Mega2 hosts the Monorepo and exposes the capture API, ScorpioFS mounts repository paths as a local filesystem, and Libra provides Agent version-control workflows and terminal browsing.

The open-source edition of Mega2 has no Web UI. Run `libra mega2 browser --server <URL>` to open Libra's terminal browser: it browses remote directories one level at a time, supports creating, deleting, moving, and renaming directories, and can list, create, and delete root tags. Write credentials are required to create or delete tags. The command does not replace Git clone, fetch, or push.

Choose a guide by task. The quick start and user guide cover day-to-day use; configuration, deployment, and development guides are for operators and contributors. Subsystem contracts and planning records remain in their own directories.

## Mega2 core capabilities

| Capability | Description | Further reading |
|---|---|---|
| Monorepo engine | Hosts code trees and Git repositories. Use Git clients for clone, fetch, and push; use Libra browser for basic remote directory browsing and tag operations. | [User Guide](user-guide.md) |
| Agent Session Capture | Optional HTTP capability for ingesting and querying Agent sessions, events, checkpoints, transcripts, and file operations. It is separate from Git push and requires explicit enablement plus an ingest token in storage-only mode. | [Agent Capture configuration and API](refactoring/agent-capture.md) · [Deployment Guide](deployment.md) |

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
