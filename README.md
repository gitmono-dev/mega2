# mega2

English · [中文](README.zh.md)

**mega2** is the storage-only successor to [Mega](https://github.com/web3infra-foundation/mega), the monorepo and Git-hosting project from the same organization. It provides a trunk-based Git storage backend.

mega2 runs only in **trunk / storage-only** mode and does not provide a Web UI. For interactive repository browsing and supported directory or tag operations, run `libra mega2 browser` from a Libra working copy.

Start with the [Quick Start](docs/quick-start.md), then see the [documentation index](docs/README.md) for user, operator, and contributor guides. Repository and push behavior is covered in the [User Guide](docs/user-guide.md); contribution checks are in the [Contributing Guide](docs/contributing.md).

## Features

### Git hosting and monorepo

- **Monorepo**: only `refs/heads/main` is accepted as a public Git branch. Other heads (for example `refs/heads/dev`) are rejected at protocol validation. The Git client sees `ng <ref> …` (`trunk push rejects ref '…'; the only public branch is refs/heads/main`). This constrains Git receive-pack / `ls-remote` heads only; Agent Capture does not use this protocol (see the Agent Capture entry below).
  - **[Trunk-based development](https://trunkbaseddevelopment.com)**: a monorepo works best with a single trunk, not a tree of long-lived feature branches.
- **No Change List in the open-source edition**: the open-source edition provides the core monorepo storage layer, without Change List. Change Lists depend on multiple branches, which this edition does not support.
- **ImportRepo**: repositories under `[monorepo].import_dir` (default `/third-party`) use ordinary Git semantics, including multiple branches and client-managed tags. This is a good place to keep third-party dependency sources that you need to modify locally and periodically sync with upstream.
- **Tags**: the Monorepo rejects `git push --tags`; create, list, and delete tags through the HTTP API. Libra provides an interactive terminal browser and the supported directory and tag operations.
- **New paths**: new paths can only be created under the roots listed in `[monorepo].root_dirs`; a Git push to a path outside them, or of history that starts from nothing (e.g. a `git init` repository) to a path that does not exist yet, is rejected with a message that starts with a `MONO_PATH_*` code. Provision the path with `mega2 path provision`, then clone and push; see [Monorepo path policy and first use](docs/user-guide.md#25-monorepo-path-policy-and-first-use).
- **Object storage**: metadata lives in Postgres, while Git objects live in object storage (local disk or an S3-compatible service). The default `object_format` is `sha1`; the optional `sha256` and `blake3` formats require [Libra](https://libra.tools).

### Protocols and large files

- **Build artifacts**: store binaries and release bundles as Artifact Sets under `/api/v1/repos/{repo}/artifacts`. Uploads use a discovery → batch → commit flow. When supported by the object-storage backend, clients transfer bytes directly with presigned URLs; otherwise, mega2 proxies the transfer. Writes use the same `git.push_auth` credentials as Git pushes, while reads are anonymous. Artifact blobs share storage with Git, LFS, and OCI objects; `[artifacts_gc]` can reclaim unreferenced artifact data in the background.
- **Git Smart HTTP and SSH**: standard Git clients can clone, fetch, pull, and push over Smart HTTP or SSH. In storage-only mode, SSH exposes read-only upload-pack; receive-pack is disabled because there is no per-user account system for SSH-key authentication. Route writes through HTTP to use one authentication model for every write surface.
- **Git LFS**: following the Git LFS standard, large files managed with git-lfs use the standard Git LFS endpoints (`/info/lfs` and `/api/v1/lfs`).

### OCI Distribution

mega2 includes an **OCI container registry** at `/v2`. Set `[oci].enabled=true` to push and pull images with standard clients such as Docker; manifests and blobs use the OCI Distribution protocol.

Image blobs **share the same object storage** as Git blobs and LFS objects (local filesystem or an S3-compatible cloud service), so there is no separate registry to deploy and operate. Registry authentication reuses the unified HTTP `push_auth` model (token or anonymous `none`) — the same credentials and path-scoped authorization semantics as Git pushes.

### Deployment

mega2 runs exclusively in **trunk / storage-only** mode (`push_policy=trunk`). A global MonoWriteQueue serializes writes to `main`, keeping the trunk history linear.

Git pushes and product write APIs (`POST /api/v1/create-entry`, `POST /api/v1/edit/save`) update the same `main` branch, so changes are immediately visible through either interface. Multi-commit pushes are squashed according to the product rules; afterward, run `git fetch && git reset --hard origin/main` to align your local checkout.

### HTTP API

- **Git hosting and Git LFS**: Git clients clone / fetch / push over the Smart HTTP protocol endpoints (`info/refs`, `git-upload-pack`, `git-receive-pack`); large files managed with git-lfs use the standard LFS endpoints (`/info/lfs`, `/api/v1/lfs`).
- **Files and directories**: read and write monorepo content over HTTP without a Git client. List directory trees, create / delete / move files or directories, and edit files online. The blob, tree, and blame endpoints return file contents, directory structures, and line-by-line history.
- **Tags**: since Git-client tag operations are forbidden in the monorepo, creation, listing, and deletion all go through these endpoints (read-only queries need no credentials).
- **OCI Distribution `/v2`**: the standard container-registry interface carrying manifest and blob uploads and pulls for `docker push` / `docker pull` (mounted when `[oci].enabled=true`).
- **Agent Capture**: captures AI coding agents' sessions, events, checkpoints, and file operations for replay and audit of agent activity (mounted when `[agent_capture].enabled=true`).

### Access control and secrets

- Storage-only writes use the configured `git.push_auth`: a static push token (`token`) or an explicitly network-restricted anonymous mode (`none`). Reads follow the Git and object-storage configuration.
- Embedded **Vault** (crates.io `libvault` + `src/contract/vault/`): PKI / KV, SecretRef, fail-closed bootstrap.

### Notifications and config

- Webhook notifications only.
- Configuration commands support initialization, validation, profiles, SecretRef, and controlled hot reload. The default file is `config/config.toml`; override it with `--config` or `MEGA_CONFIG`. See the [Configuration Guide](docs/configuration.md) for available settings and validation.

## Quick start with Compose

The evaluation stack pulls the official release image from Docker Hub (`genedna/mega2:latest`) — no source build, no bootstrap. Pick the compose file for your platform (they differ in how artifact presigned URLs are made reachable from the host — see [`docs/deployment.md`](docs/deployment.md)):

```bash
# macOS with OrbStack
docker compose -f macos-orbstack-mega2-compose.yml up -d --wait
# Linux Docker
docker compose -f linux-mega2-compose.yml up -d --wait
```

HTTP: `http://127.0.0.1:9000/`. This is a local-only anonymous setup (`push_auth=none`, bound to `127.0.0.1`); for token-based or shared deployments see the [Deployment Guide](docs/deployment.md). Start with the [`Quick Start`](docs/quick-start.md); for migration, LFS, OCI, and artifact examples, see [`Usage Recipes`](docs/recipes.md).

For interactive browsing, run `libra mega2 browser` in a Libra working copy. It provides the terminal experience for repository navigation and supported directory / tag operations; mega2 itself does not serve a Web UI.

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
| [`docs/contributing.md`](docs/contributing.md) | Local development and integration tests |
| [`docs/contributing.md`](docs/contributing.md) | Contribution process and code conventions |

Configuration, deployment, architecture, and subsystem references are linked from the [documentation index](docs/README.md).
