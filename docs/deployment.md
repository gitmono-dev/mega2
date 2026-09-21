English · [中文](deployment.zh.md)

# Deployment Guide

The open-source edition of mega2 ships in exactly one shape: **trunk / storage-only** — no Web UI, no Change List; interactive browsing is done via Libra's `libra mega2 browser`. This document is the install and go-live guide for that shape. The operational fact source for runtime behavior, auth semantics, and morphology switching is [`deploy-trunk.md`](./deploy-trunk.md); product rules live in [`monorepo.md`](./monorepo.md); configuration keys live in [`config/config.toml`](../config/config.toml) (heavily commented sample) and [`refactoring/config.md`](./refactoring/config.md). This guide does not restate those facts — it orients and walks you through the concrete steps.

## 1. Deployment shape

The only supported deployment shape is trunk / storage-only:

- Single public branch `main`; all writes (git push and product API writes) are globally serialized through the MonoWriteQueue and share tip authority; no CL / issue / reviewer / OAuth user routes are registered.
- HTTP surface: Git smart HTTP (`info/refs`, `git-upload-pack`, `git-receive-pack`), LFS (`/info/lfs`, `/api/v1/lfs`), storage-only `/api/v1/*` (status, file/blob, file/tree, preview reads; create-entry / delete-entry / move-entry / edit/save, tags writes), optional OCI `/v2` (`[oci].enabled`), optional Agent Capture `/api/v1/agent-capture` (`[agent_capture].enabled`), Swagger UI `/swagger-ui`, OpenAPI `/api/openapi.json`.
- SSH is upload-pack only (clone / fetch / pull); `ssh_receive_pack` must be explicitly `false` — omitting it refuses startup.
- Write auth: `git.push_auth = "token"` (recommended) or `"none"` (controlled networks only). Auth semantics, the fail-closed checklist, and SSH details: [`deploy-trunk.md`](./deploy-trunk.md) §1–§4.

## 2. Compose deployment

The Compose files at the repository root fall into two groups: the **official evaluation stack** and the **test / lab stacks**.

### 2.1 Evaluation stacks (local trial, recommended entry)

The evaluation stack ships in two platform variants; both pull the **official release image from Docker Hub**, `genedna/mega2:latest` (`pull_policy: always`) — no source build. Components: mega2 + Postgres + Redis + RustFS + `rustfs-init` (creates the `mega2` bucket automatically); mega2 initializes the empty Monorepo during service startup, so **no** `service init` bootstrap is needed.

- [`macos-orbstack-mega2-compose.yml`](../macos-orbstack-mega2-compose.yml) — macOS with OrbStack. Uses OrbStack's `*.orb.local` DNS so that the RustFS endpoint (`http://mega2-rustfs.orb.local:9000`) resolves both on the host and inside containers.
- [`linux-mega2-compose.yml`](../linux-mega2-compose.yml) — Linux Docker. Publishes Postgres/Redis/RustFS on the host loopback and runs mega2 with `network_mode: host`, so `http://127.0.0.1:29000` is reachable from both sides.

The two variants exist because artifact **presigned URLs** carry the object-storage endpoint host in their SigV4 signature: one address must serve both the mega2 process (SDK + presigning) and clients on the host (direct blob download), and each platform provides a different mechanism for that shared name.

```bash
docker compose -f macos-orbstack-mega2-compose.yml up -d --wait   # macOS + OrbStack
docker compose -f linux-mega2-compose.yml up -d --wait            # Linux Docker
docker compose -f <file> logs -f mega2
docker compose -f <file> down      # named volumes keep the data; down -v wipes it
```

Both stacks are a **local-only, anonymous setup**: `push_auth=none`, anonymous reads and writes, and HTTP is bound to `127.0.0.1:9000` only. Do not rebind to `0.0.0.0` or expose it through a reverse proxy; for shared / public deployments use the token-based stack below or your own orchestration. An end-to-end walkthrough: [`quick-start.md`](./quick-start.md).

### 2.2 Source-built test / lab stack: `docker/docker-compose-storage-only.yml`

[`docker/docker-compose-storage-only.yml`](../docker/docker-compose-storage-only.yml) is the local / lab reference stack that **builds** the image from source (for deployment rehearsals and smoke tests — not the official distribution form): mega2 + Postgres + Redis + RustFS, mounting [`config/config-storage-only.toml`](../config/config-storage-only.toml). It can coexist with the IT stack `docker/docker-compose.test.yml` (no port conflicts).

Services and host ports (authoritative source is the compose file):

| Service | Container port | Host publish | Notes |
| --- | --- | --- | --- |
| mega2 HTTP | 8000 | `127.0.0.1:9000` | Git smart HTTP + LFS + storage-only API + Swagger UI |
| mega2 SSH | 2222 | `127.0.0.1:2222` | upload-pack only (receive-pack disabled via config) |
| postgres | 5432 | `127.0.0.1:25432` | |
| redis | 6379 | `127.0.0.1:26379` | |
| rustfs API | 9000 | `127.0.0.1:29000` | default S3-compatible object store |
| rustfs console | 9001 | `127.0.0.1:29001` | |
| git-smoke | — | no published ports | `--profile smoke` black-box smoke (see [`deploy-trunk.md`](./deploy-trunk.md) §8.1) |

First start and bootstrap:

```bash
# First build (context = repo root)
docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml build mega2

# Start (default RustFS; do NOT add --env-file)
docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml up -d --wait

# Empty-volume bootstrap (one-shot; creates the initial graph, starts no listeners)
docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml exec -T mega2 \
  mega2 --config /etc/mega2/config.toml service init --yes
```

**Push token secret**: compose mounts the token file as `/run/secrets/mega2-push-token`, defaulting to `./secrets/mega2-push-token.local` (`secrets/` is gitignored — create it yourself). For real deployments point `MEGA2_PUSH_TOKEN_FILE=/path/to/secret` at the real secret file; never commit plaintext. Token configuration and `paths` authorization semantics: [`deploy-trunk.md`](./deploy-trunk.md) §3.

### 2.2.1 `push_auth=none` override variant

[`docker/docker-compose-storage-only.auth-none.yml`](../docker/docker-compose-storage-only.auth-none.yml) is an opt-in override: compose it with the base file via two `-f` flags to remount mega2's config as `config/config-storage-only.none.toml`, and `--force-recreate mega2`:

```bash
docker compose -p mega2-trunk \
  -f docker/docker-compose-storage-only.yml \
  -f docker/docker-compose-storage-only.auth-none.yml \
  up -d --wait --force-recreate mega2
```

> **Warning**: `push_auth=none` means anonymous receive-pack **and anonymous LFS upload**. It is only suitable for controlled internal networks, loopback, or deployments behind a Unix socket front. Never expose it publicly. To revert to token auth: drop the second `-f` and `--force-recreate mega2` again.

### 2.2.2 Object storage switch

The default backend is RustFS (`s3compatible`) — plain `up` works. Add `--env-file` only when switching mega2 to the local filesystem backend (the RustFS container still starts; only mega2's `storage_type` changes):

```bash
docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml \
  --env-file config/compose.env.storage-only.local up -d --wait
```

Env file contents: [`config/compose.env.storage-only.local`](../config/compose.env.storage-only.local); backend contract: [`refactoring/orbit.md`](./refactoring/orbit.md).

## 3. Binary / container deployment

If you don't want to build from source, use the official release image on Docker Hub, `genedna/mega2:latest` (the same image the evaluation compose stacks pull). To build the binary yourself:

```bash
cargo build --release -p mega2   # artifact: target/release/mega2
```

[`Dockerfile`](../Dockerfile) containerizes the same artifact: two stages (`rust:1.97-bookworm` builder → `debian:bookworm-slim` runtime), config copied to `/etc/mega2/config.toml`, `MEGA_BASE_DIR=/var/lib/mega2`, `EXPOSE 8000`, `ENTRYPOINT` is mega2, default `CMD` is `service http --host 0.0.0.0 -p 8000`.

Choose the service shape as needed (full CLI surface in `src/commands/mod.rs` and [`AGENTS.md`](../AGENTS.md)):

- `service http --host 0.0.0.0 -p 8000` — HTTP only (Dockerfile default).
- `service ssh` — SSH only (upload-pack).
- `service multi http ssh -p 8000 --ssh-port 2222` — HTTP + SSH in a single process (the compose stack's usage).

Process management: mega2 is a single long-running process; manage it with systemd or a container restart policy. All state lives in the external dependencies (Postgres / Redis / object storage) and the `MEGA_BASE_DIR` data directory. Config hot reload is a 5-second polling watcher; only whitelisted fields apply live, everything else reports `restart_required` (see [`src/config/reload.rs`](../src/config/reload.rs)) — watch the logs after config edits and restart when required.

## 4. External dependencies & minimum requirements

| Dependency | Notes |
| --- | --- |
| PostgreSQL | Required. The compose stack uses 18.x. |
| Redis | Required. The compose stack uses 8.x. |
| Object storage | Required, one of: local filesystem (`storage_type="local"`, single node only), S3-compatible (default; RustFS / MinIO / cloud S3), GCS. Built via `build_object_storage` per [`refactoring/orbit.md`](./refactoring/orbit.md). |
| Vault | No external service: the embedded Vault comes from crates.io `libvault` + [`src/contract/vault/`](../src/contract/vault/), see [`refactoring/vault.md`](./refactoring/vault.md). |

Resource sizing scales with repository size and push concurrency; the write path is globally serialized (MonoWriteQueue), and read-path horizontal scaling is bounded by Postgres / object storage. Validate configuration before deploying with `mega2 --config <path> config validate` (optionally `--show-sources` / `--deny-warnings`); see [`refactoring/config.md`](./refactoring/config.md).

## 5. Production hardening checklist

- [ ] `push_auth = "token"`, with `[[git.push_tokens]]` `paths` narrowed to the minimum component boundaries; **never** expose `push_auth=none` publicly (see the §2.1 warning and [`deploy-trunk.md`](./deploy-trunk.md) §3).
- [ ] Inject credentials via `${file:...}` file mounts or Vault SecretRefs; the committed `config.toml` contains no plaintext (sample: [`config/config.toml`](../config/config.toml)).
- [ ] Terminate TLS at a reverse proxy; point `MEGA_HTTP__PUBLIC_BASE_URL` and the LFS URLs at the external https address (plaintext HTTP registries need an insecure-registry client config; see [`refactoring/oci.md`](./refactoring/oci.md)).
- [ ] `log.print_std = false` so logs land under `mega_cache()/logs`; the compose stack's `print_std=true` suits containers only.
- [ ] Bind host ports to loopback / internal addresses; open only what is needed.
- [ ] Keep `cedar.enforcement` at `off` (a trunk-morphology startup precondition; see [`deploy-trunk.md`](./deploy-trunk.md) §1).
- [ ] Enable OCI `/v2` and Agent Capture explicitly, only when needed (not mounted by default).
- [ ] Backups, three parts: Postgres dump, object storage bucket, `mega2 --config <path> config vault backup <destination>` (Vault core key; restore with `config vault restore`, see [`src/commands/config.rs`](../src/commands/config.rs)).

## 6. Upgrades & morphology switching

- Upgrades: swap the image / binary and restart. Config changes marked `restart_required` take effect only after a restart.
- Morphology switching (`review ↔ trunk`) — startup preconditions (no open CLs, drained `push_queue`, explicit `push_auth`) and index watermark reset: the operational fact source is [`deploy-trunk.md`](./deploy-trunk.md) §5; design rationale in [`refactoring/trunk-push.md`](./refactoring/trunk-push.md).

## 7. Observability

- Health: `GET /api/v1/status`; the compose healthcheck uses `GET /api/openapi.json`.
- Logs: hourly rolling files under `mega_cache()/logs` (stdout when `log.print_std=true`, as in the compose stack).
- API catalog: Swagger UI `/swagger-ui`, OpenAPI JSON `/api/openapi.json`.
- Stack-level smoke: `scripts/git_protocol_smoke_storage_only.sh` (Git protocol) and `scripts/api_write_smoke_storage_only.sh` (API write → Git visibility), usage in [`deploy-trunk.md`](./deploy-trunk.md) §8.1; OCI smoke `scripts/oci_smoke_storage_only.sh` ([`deploy-trunk.md`](./deploy-trunk.md) §10.2).

## 8. Further reading

- This documentation set: [`quick-start.md`](./quick-start.md) · [`user-guide.md`](./user-guide.md) · [`configuration.md`](./configuration.md) · [`architecture.md`](./architecture.md) · [`contributing.md`](./contributing.md)
- [`deploy-trunk.md`](./deploy-trunk.md) — operational fact source for trunk / storage-only (auth, SSH, LFS, morphology switching, OCI, smoke).
- [`monorepo.md`](./monorepo.md) — product rules (single public branch, invariants, tag restrictions).
- [`development.md`](./development.md) — local development and testing.
- [`config/config.toml`](../config/config.toml) + [`refactoring/config.md`](./refactoring/config.md) — configuration keys and load / validation semantics.
- [`refactoring/orbit.md`](./refactoring/orbit.md), [`refactoring/vault.md`](./refactoring/vault.md), [`refactoring/oci.md`](./refactoring/oci.md), [`refactoring/agent-capture.md`](./refactoring/agent-capture.md) — subsystem contracts.
