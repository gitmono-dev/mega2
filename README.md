# mega2

English · [中文](README.zh.md)

The first-generation [Mega](https://github.com/web3infra-foundation/mega) established the foundation for our monorepo and Git-hosting platform. **Mega2 is the second-generation engine built for AI agents**, with two core capabilities: a **Monorepo engine** and **Agent Session Capture**.

The open-source edition of Mega2 runs in **trunk / storage-only** mode and does not include a Web UI or Change List. Mega2 provides the server-side monorepo storage and APIs; Libra provides a terminal interface for interactive browsing.

Start with the [Quick Start](docs/quick-start.md), then see the [documentation index](docs/README.md) for user, operator, and contributor guides. Repository and push behavior is covered in the [User Guide](docs/user-guide.md); contribution checks are in the [Contributing Guide](docs/contributing.md).

## Recommended Agent setup

The best practice for agent-oriented monorepo development is to use **Mega2 + ScorpioFS + Libra** together:

- **Mega2** provides centralized monorepo storage and hosting, with optional Agent Session Capture.
- **[ScorpioFS](https://github.com/gitmono-dev/scorpiofs)** mounts remote monorepo paths as a local filesystem for development tools and agents to browse.
- **[Libra](https://libra.tools)** provides agent-side version-control workflows and a terminal browser for Mega2.

Use `libra mega2 browser --server https://mega2.example.com` to browse a remote monorepo. The terminal interface displays one directory level at a time, supports creating, deleting, moving, and renaming directories, and has a Tag panel for listing, creating, and deleting root tags; write credentials are required to create or delete tags. This command reads remote tree data; it does not run Git clone, fetch, or push. See [Using Mega2 with Libra](docs/user-guide.md#6-using-mega2-with-libra).

## Core capabilities

### Monorepo engine

Mega2 provides trunk-based Git storage and hosting for monorepos. Regular Monorepo paths expose `refs/heads/main` as the public trunk; Git clients cannot push other branches or tags. Use the HTTP API or Libra's terminal browser for supported Tag and directory operations. The open-source edition does not include the multi-branch Change List feature.

Git metadata is stored in Postgres, while Git objects live in local or S3-compatible object storage. ImportRepos under `[monorepo].import_dir` (default `/third-party`) follow ordinary Git semantics, including multiple branches and client-managed tags, and can hold third-party repositories that need independent synchronization. ImportRepos accept later pushes (branches and tags) and can be removed through `POST /api/v1/import-repo/remove` (which needs a push token; `push_auth=none` deployments run `mega2 import-repo remove` on the server instead); see [ImportRepo lifecycle](docs/user-guide.md#26-importrepo-lifecycle). See [Monorepo path policy and first use](docs/user-guide.md#25-monorepo-path-policy-and-first-use) for path provisioning and push rules.

### Agent Session Capture

Agent Session Capture is an optional HTTP capability, separate from Git push. When enabled with `[agent_capture].enabled=true` and an ingest token in a storage-only deployment, Mega2 can capture agent sessions, events, checkpoints, file operations, and transcript data for later querying, review, and audit. Git branch and receive-pack rules do not govern this API. See the [Configuration Guide](docs/configuration.md) for setup.

## Other capabilities

- **Git and large files**: Git Smart HTTP supports clone, fetch, pull, and push; SSH is read-only in storage-only mode; Git LFS is supported.
- **Build artifacts and OCI**: Artifact Sets store build outputs. When `[oci].enabled=true`, the `/v2` endpoint provides an OCI container registry. Git, LFS, Artifact, and OCI data can share object storage.
- **HTTP API**: Read and write monorepo files, directories, and tags. See the [User Guide](docs/user-guide.md) for the API and workflows.
- **Security and operations**: Push-token authentication, embedded Vault and SecretRef, webhook notifications, and configuration validation, profiles, and controlled hot reload. See the [Configuration Guide](docs/configuration.md) and [Deployment Guide](docs/deployment.md).

## Quick start with Compose

The evaluation stack pulls the official release image from Docker Hub (`genedna/mega2:latest`) — no source build, no bootstrap. Pick the compose file for your platform (they differ in how artifact presigned URLs are made reachable from the host — see [`docs/deployment.md`](docs/deployment.md)):

```bash
# macOS with OrbStack
docker compose -f macos-orbstack-mega2-compose.yml up -d --wait
# Linux Docker
docker compose -f linux-mega2-compose.yml up -d --wait
```

HTTP: `http://127.0.0.1:9000/`. This is a local-only anonymous setup (`push_auth=none`, bound to `127.0.0.1`); for token-based or shared deployments see the [Deployment Guide](docs/deployment.md). Start with the [`Quick Start`](docs/quick-start.md); for migration, LFS, OCI, and artifact examples, see [`Usage Recipes`](docs/recipes.md).

For local development and tests, see the [Contributing Guide](docs/contributing.md).

### Stop

The `-v` flag deletes data stored in the Docker volumes.

```bash
docker compose -f macos-orbstack-mega2-compose.yml down -v   # or linux-mega2-compose.yml
```

## Contributing

For a large change, follow this process:

1. **Open an issue.** Describe the problem, motivation, scope, and non-goals. Get agreement on the direction before implementation.
2. **Then write a plan.** Copy the structure from the [English Plan Template](docs/plan/plan-template.en.md) into `docs/plan/plan-YYYYMMDD.md`. Do not delete mandatory sections; write `N/A` and the reason when a section does not apply.
3. **Implement only after the plan is reviewed.** Split work into task cards, add tests and docs, and pass the submit gates before merge.

A plan is not an implementation. When drafting one, verify assumptions against the current source, tests, config, and docs. Agreements in an issue do not replace the task card's verification commands.

Before submitting code, run:

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
source .env.test && cargo test --all
```

Details: [`AGENTS.md`](AGENTS.md) and the [Contributing Guide](docs/contributing.md).

## Documentation

| Start here | What it covers |
|---|---|
| [`docs/README.md`](docs/README.md) | Index of user, operator, and developer documentation |
| [`docs/quick-start.md`](docs/quick-start.md) | Start the local Compose stack and make your first push |
| [`docs/recipes.md`](docs/recipes.md) | Repository migration, LFS, OCI, artifacts, and persistent data |
| [`docs/user-guide.md`](docs/user-guide.md) | Git, HTTP API, Libra, and CLI usage |
| [`docs/contributing.md`](docs/contributing.md) | Local development, contribution process, code conventions, and integration tests |

Configuration, deployment, architecture, and subsystem references are linked from the [documentation index](docs/README.md).
