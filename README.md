# mega2

English · [中文](README.zh.md)

**mega2** is the successor engine to the same organization's [Mega](https://github.com/web3infra-foundation/mega) project: it continues Mega's shipped monorepo / Git-hosting work as a storage-only code-hosting backend.

mega2 supports one deployment mode: trunk / storage-only. It does not include a Web UI or megaui integration. For interactive repository browsing and the corresponding directory / tag operations, use Libra's `libra mega2 browser` command.

Product rules: [`docs/monorepo.md`](docs/monorepo.md). Storage-only deploy: [`docs/deploy-trunk.md`](docs/deploy-trunk.md). Local development and tests: [`docs/development.md`](docs/development.md).

## Features

### Git hosting and monorepo

- **Monorepo**: only `refs/heads/main` is accepted as a public Git branch. Other heads (for example `refs/heads/dev`) are rejected at protocol validation. The Git client sees `ng <ref> …` (`trunk push rejects ref '…'; the only public branch is refs/heads/main`). This constrains Git receive-pack / `ls-remote` heads only; Agent Capture does not use this protocol (see the Agent Capture entry below).
  - **[Trunk-based development](https://trunkbaseddevelopment.com)**: a monorepo works best with a single trunk, not a tree of long-lived feature branches.
  - **No Change List in the open-source edition**: The Mega2 open-source edition ships the core monorepo storage capability and does not include Change List. A Change List implementation needs multiple branches, so this edition does not have multi-branch capability.
- **ImportRepo**: under `[monorepo].import_dir` (default `/third-party`), ordinary Git multi-branch and client tags apply. This is a Mega2 feature for developers to store the source of open-source third-party dependency libraries so they can use the latest versions of those libraries in local development.
- **Tags**: Monorepo forbids `git push --tags`. Create / list / delete go through the HTTP API only. Mega2 is meant to be used together with Libra as the version-control tool. Use `libra mega2 browser` for an interactive terminal interface to browse repositories and manage directories, tags, and similar operations.
- **Object graph**: metadata in Postgres; blobs in pluggable object storage (local filesystem or S3-compatible object storage). `object_format` supports `sha1` (default) and the extensions `sha256` / `blake3` (these features require Libra as the version-control tool).

### Protocols and large files

- **Git Smart HTTP and SSH**: Mega2 speaks Smart HTTP and SSH to stock Git clients for clone / fetch / pull / push. Storage-only disables SSH receive-pack; SSH remains available for read-only fetch.
- **Git LFS**: large files are kept out of the ordinary Git object graph and use stock Git LFS (`/info/lfs` and `/api/v1/lfs`).
- **FastCDC Media**: on top of stock LFS, large media can be uploaded and reused as content-defined chunks (`--features fastcdc`). FastCDC and BLAKE3 support are Monorepo features built for large files and hash safety; they require Libra. Contract: [`docs/refactoring/fastcdc-media.md`](docs/refactoring/fastcdc-media.md).

### Deployment

mega2 is deployed exclusively in **trunk / storage-only** mode: `push_policy=trunk`; pushes enter `main` through `MonoWriteQueue`.

The product write APIs (`POST /api/v1/create-entry`, `POST /api/v1/edit/save`) share tip authority with `git push`. Root-tree writes are globally serialized. Multi-commit pushes merge into `main` per product rules.

### HTTP API

- Git hosting and Git LFS.
- File and directory reading, creation, and editing, plus blob / tree / blame browsing.
- Tag creation, listing, and deletion.
- OCI Distribution `/v2` manifest / blob push and pull (when `[oci].enabled=true`).
- Agent Capture session, event, checkpoint, and file-operation capture (when `[agent_capture].enabled=true`).

### Access control and secrets

- Storage-only writes use the configured `git.push_auth`: a static push token (`token`) or an explicitly network-restricted anonymous mode (`none`). Reads follow the Git and object-storage configuration.
- Embedded **Vault** (crates.io `libvault` + `src/contract/vault/`): PKI / KV, SecretRef, fail-closed bootstrap.

### Notifications and config

- Webhook notifications only.
- First-class config module: `config init` / `validate` / secret, Profile, SecretRef, controlled hot reload. Default file `config/config.toml`, overridable with `--config` or `MEGA_CONFIG`.

## Quick start with Compose

Git HTTP + object storage:

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml up -d --wait

docker compose -p mega2-trunk -f docker-compose-storage-only.yml exec -T mega2 \
  mega2 --config /etc/mega2/config.toml service init --yes
```

HTTP: `http://127.0.0.1:9000/`. Default push token: [`docs/deploy-trunk.md`](docs/deploy-trunk.md).

For interactive browsing, run `libra mega2 browser` in a Libra working copy. It provides the terminal experience for repository navigation and supported directory / tag operations; mega2 itself does not serve a Web UI.

For local development and tests, see [`docs/development.md`](docs/development.md).

### Stop

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml down -v
```

## Contributing

Do not start a large change cold. The order is:

1. **Open an Issue first.** State the problem, motivation, scope, and explicit non-goals. Wait until maintainers (or the discussion) accept the direction.
2. **Then write a plan.** Copy the structure from the [Chinese Plan Template](docs/plan/plan-template.md) (in-repo operational original) or the [English Plan Template](docs/plan/plan-template.en.md) into `docs/plan/plan-YYYYMMDD.md`. Do not delete mandatory sections; write `N/A` and the reason when a section does not apply.
3. **Implement only after the plan is reviewed.** Split work into task cards, add tests and docs, and pass the submit gates before merge.

A plan is not an implementation. At write time, the fact baseline is the current checkout: source, tests, config, and docs. Verbal Issue agreements do not replace the verification commands on a task card.

Before submit, at least:

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
source .env.test && cargo test --all
```

Details: [`AGENTS.md`](AGENTS.md) and [`docs/plan/README.md`](docs/plan/README.md).

## Docs

| Doc | Contents |
|---|---|
| [`README.zh.md`](README.zh.md) | Chinese product README |
| [`docs/monorepo.md`](docs/monorepo.md) | Monorepo product rules |
| [`docs/deploy-trunk.md`](docs/deploy-trunk.md) | trunk / storage-only deploy |
| [`docs/development.md`](docs/development.md) | Local development and tests |
| [`docs/manual/authz.md`](docs/manual/authz.md) | Authn / authz operations |
| [`docs/errors.md`](docs/errors.md) | Error contract |
| [`docs/refactoring/agent-capture.md`](docs/refactoring/agent-capture.md) | Agent Capture HTTP / tables / object namespace |
| [`docs/refactoring/storage-events.md`](docs/refactoring/storage-events.md) | Post-commit outbound events |
| [`docs/plan/`](docs/plan/) | Dated plans and the long-term roadmap |
| [`docs/refactoring/`](docs/refactoring/) | Other module contracts and implementation fact sources |
| [`docs/plan/plan-template.en.md`](docs/plan/plan-template.en.md) | English Plan Template for contributors |
