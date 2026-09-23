English · [中文](architecture.zh.md)

# Architecture

This document maps mega2's modules, storage layers, write path, protocol surfaces, and configuration reload flow. It reflects the source in this checkout; inline `src/...` paths point to the relevant implementation.

> **Scope:** the open-source service is deployed in trunk / storage-only mode. It has no Web UI; use Libra's `libra mega2 browser` for interactive browsing. The [User Guide](./user-guide.md) covers repository paths, branches, tags, and push behavior; the [Deployment Guide](./deployment.md) covers installation and operations. This page provides an architecture overview and links to implementation references.

## 1. Overview and dependency flow

mega2 is a single Cargo package (lib `mega2_core` + binaries; see [`../Cargo.toml`](../Cargo.toml)). It ports and refactors selected parts of the upstream Mega project; it is not a mirror, and the two repositories do not share identical module boundaries. Evaluate upstream changes against this checkout's code and dependency lockfile before adopting them. Entry point: `src/main.rs` → `cli::parse` → the subcommand registry in `src/commands/mod.rs`. Runtime dependency flow is one-directional: upper layers compose lower layers; lower layers never reference back up.

```
┌────────────────────────────────────────────────────────────┐
│ CLI (src/commands)                                         │
│   service(init/http/ssh/multi) · config · debug · authz-audit │
└───────────────────────────┬────────────────────────────────┘
                            │ assembly (AppContext::new staged bootstrap)
                            ▼
┌────────────────────────────────────────────────────────────┐
│ context::AppContext (composition root, src/context/mod.rs) │
│   Storage · VaultCore · ConfigHandle · redis ConnectionManager │
│   · SharedEntityStore · shutdown tokens                      │
└───────────────────────────┬────────────────────────────────┘
                            │
              ┌─────────────┴─────────────┐
              ▼                           ▼
┌──────────────────────────┐   ┌────────────────────────────┐
│ server::http_server      │   │ server::ssh_server         │
│ (axum router assembly)   │   │ (read-only in storage-only)│
└───────────┬──────────────┘   └───────────┬────────────────┘
            ▼                              │
┌──────────────────────────┐               │
│ api routers / contract:: │◄──────────────┘
│ git_protocol             │
└───────────┬──────────────┘
            ▼
┌──────────────────────────┐
│ ceres (business services:│
│ pack, api_service, lfs…) │
└───────────┬──────────────┘
            ▼
┌────────────────────────────────────────────────────────────┐
│ jupiter storage                                            │
│   ├─ PostgreSQL (sea-orm metadata, callisto entities)      │
│   ├─ object storage (orbit_api contract + orbit backends:  │
│   │    local / S3 / GCS)                                   │
│   └─ Redis (cache / distributed locks)                     │
└────────────────────────────────────────────────────────────┘
```

Key points:

- **`AppContext` is the single composition root** (`src/context/mod.rs:41`). `AppContext::new` bootstraps in stages: `Config::validate` first (fail-closed), then a DB connection → a DB-only `VaultCore` bootstrap → resolution of `vault://` SecretRefs for object storage / Redis → `build_object_storage` → `Storage` → Redis → the notification worker → `init_monorepo` and background tasks (push queue reaper, blob path compensator, tombstone audit). Any stage failing fails startup as a whole.
- **Services do not hold config directly**: `ConfigHandle` (`src/config/reload.rs`) holds the hot-reloadable snapshot, and `AppContext::config()` prefers the snapshot (hot reload in §7).
- **Read-only assembly is a separate path**: read-only ops commands such as `authz-audit` go through `ReadOnlyContext` (`src/context/mod.rs:516`) — a read-only DB connection (no migrations) and a vault opened read-only only when needed, deliberately isolated from the production assembly.

For implementation details, start from the module paths listed in this guide and the development workflow in the Contributing Guide.

## 2. Module responsibilities

| Module (`src/`) | Responsibility |
| --- | --- |
| `commands` | CLI subcommand registry and executors (`service` / `config` / `debug` / `authz-audit`); global flags `--config` / `--profile` (env `MEGA_CONFIG` / `MEGA_PROFILE`). The how-to for adding a subcommand lives in [`contributing.md`](./contributing.md) |
| `common` | Error types (`MegaError` / `MegaResult`), utilities, `canonical_json`, `oci_name` |
| `config` | TOML config pipeline: `loader` (source resolution), `model` (config model), `validate` (startup validation), `secret` (SecretRef), `reload` (hot reload). Note: the config module is `src/config/`, **not** `src/common/config` |
| `context` | The composition root `AppContext` and the read-only assembly `ReadOnlyContext` (see §1) |
| `server` | Service bootstrap: `http_server` (axum router assembly, CORS/session/trace middleware), `ssh_server`, `trace_context` |
| `api` | HTTP handlers and routes: `api_router` (mode-specific `/api/v1` routes), `lfs_router`, `oci_router`, `preview_router`, `tag_router`, `agent_capture_router`, `api_write_auth` (product-write auth), `api_doc` (OpenAPI) |
| `api_model` | Request/response DTOs (utoipa schemas) |
| `ceres` | Business services: `protocol` + `pack` (Git smart protocol and pack handling), `api_service` (monorepo read/write core), `lfs`, `oci`, `agent_capture`, `code_edit`, `snapshot` (MST/2), `github_sync`, `merge_checker` |
| `jupiter` | Storage and service foundation: `storage/` (`*Storage` wrappers), `service/` (`push_queue_service` and friends), `migration/` (sea-orm-migration), `redis/` |
| `callisto` | sea-orm entities, one file per table; re-exported via `pub use crate::callisto::*` in `lib.rs` |
| `orbit_api` + `orbit` | Object-storage contract (traits/config) and backend implementations (local / S3 / GCS), built through `orbit::factory::ObjectStorageFactory` |
| `contract` | Cross-layer contracts: `git_protocol` (smart-protocol mounts and auth context), `policy` (Cedar authorization and entity snapshots), `vault` (product integration layer over libvault), `api` |
| `notification` | Notification dispatch: `NotificationService`, channels (in-app / webhook), triggers |

Detailed layout and conventions (import grouping, error types, comment density) are in [`../AGENTS.md`](../AGENTS.md), sections Project Layout and Code Conventions.

## 3. Storage architecture

Three storage kinds with distinct roles, all accessed through the `jupiter` layer — API/handler code never touches `sea_orm` directly ([`../AGENTS.md`](../AGENTS.md) convention):

1. **Metadata: PostgreSQL + sea-orm** (SQLite for tests). Table entities live in `src/callisto/` (one file per table); above them, one `*Storage` wrapper per domain (`src/jupiter/storage/`: `mono_storage`, `git_db_storage`, `lfs_db_storage`, `push_queue_storage`, `vault_storage`, …), aggregated into `Storage` (`src/jupiter/storage/mod.rs`) over a shared `BaseStorage` connection. Schema evolution goes through the sea-orm-migration migrators in `src/jupiter/migration/`.
2. **Object content: pluggable object storage**. Git objects and LFS object bytes are not stored in the database; `build_object_storage` (`src/jupiter/storage/object_storage.rs`) builds the backend through the orbit factory per `[object_storage].storage_type`: local / S3-compatible / GCS. Contract types live in `src/orbit_api/`, implementations in `src/orbit/`. S3-family credentials accept `vault://` SecretRefs (see §5). Large LFS objects may use FastCDC chunking; the implementation lives under `src/ceres/lfs/`.
3. **Redis: cache and distributed locks, not a queue**. The git object cache (`GitObjectCache`) and the `RedLock` mutex live in Redis (`src/jupiter/redis/`); the write queue is the Postgres `push_queue` table (see §4) — there is no FIFO/durable queue on the Redis side. The connection is shared via `connection-manager`, and `redis.url` accepts a SecretRef.

See the [Configuration Guide](./configuration.md) for storage-related settings and the [Deployment Guide](./deployment.md) for operating the service.

## 4. The write path: MonoWriteQueue

**All writes to `main` are globally serialized by MonoWriteQueue** (`src/jupiter/service/push_queue_service.rs`, backed by the Postgres `push_queue` table + `PushQueueService`). Git push, product API writes (`create-entry` / `delete-entry` / `move-entry` / `edit/save`), CL merges, and import attach share a single tip authority — queue order *is* `main`'s advance order, and no second write path can bypass it. A direct consequence: concurrent pushes land one by one in enqueue order, and conflicts are re-checked against the *current* tip at execution time rather than an enqueue-time snapshot.

**Review and trunk modes share the same queue. They differ in whether a push passes through the CL pipeline first.** In review mode (the default), branch pushes become CLs (`refs/cl/*`) and enter the queue after review and merge. In trunk mode (the mode shipped by this repository), pushes and product API writes enter the queue directly and advance path tips without creating CLs. Mode-switch checks fail closed: Cedar must be off, there must be no open CLs, the queue must be drained, and `push_auth` must be explicit. Violations stop startup; they are not warnings. The [Deployment Guide](./deployment.md) describes the shipped service shape and startup requirements. Both `Config::validate` and `AppContext::new` enforce these checks (`src/context/mod.rs:128`), including service paths that bypass the CLI.

For user-visible push behavior, see the [User Guide](./user-guide.md); deployment requirements are in the [Deployment Guide](./deployment.md).

## 5. Authentication, authorization, and secrets

Authentication, authorization, and secrets serve distinct purposes:

- **Push authentication (authn)**: `git.push_auth = token | none`. Static tokens with constant-time lookup; `paths` prefixes authorize at **component boundaries** (`/project/foo` does not cover `/project/foobar`). Git receive-pack, LFS batch/lock writes, and product API writes share this model; the authenticated identity is separate from the commit author (author is self-declared provenance and takes no part in decisions). The [User Guide](./user-guide.md) explains the available authentication modes and write surfaces.
- **Authorization decisions (authz)**: Cedar (`cedar-policy`; schema `src/contract/policy/mega.cedarschema`, policies `src/contract/policy/mega_policies.cedar`) has three modes: `off / shadow / enforce`. Enforcement applies only in review mode; trunk requires `cedar.enforcement = "off"` and fails startup otherwise. The write path (notify) and read path (guard/push) share one authorization snapshot through `SharedEntityStore` (`src/context/mod.rs:69`). These modes and their scope are summarized here; trunk deployments require `off`.
- **Secrets management**: an embedded Vault — the library comes from the crates.io `libvault` crate (the vendored module was removed on 2026-08-21), and the product integration layer is `src/contract/vault/` (`VaultCore` / `VaultSecretResolver`). Secrets in config are written as `vault://` SecretRefs (`src/config/secret.rs`) for a whitelist of fields — object-storage S3 credentials, `redis.url`, the notification webhook token, storage_events HMAC secrets, etc.; each field is bound to a fixed vault namespace, a resolution failure at startup fails startup, and resolved values are never written back into the config snapshot nor surfaced in error messages. The `config secret` / `config vault` subcommands are its CLI surface.

See the [Configuration Guide](./configuration.md) for SecretRef settings and validation.

## 6. Protocol layer: mount points and switches

`server::http_server::app` assembles the HTTP router for the configured mode (`src/server/http_server.rs:681`). The table lists each surface, its route, and its gate:

| Surface | Mount point | Switch |
| --- | --- | --- |
| Git smart HTTP | `*/info/refs`, `*/git-upload-pack`, `*/git-receive-pack` (catch-all fallback) | always mounted |
| SSH | `service ssh` (dedicated port) | read-only (upload-pack) in storage-only; `git.ssh_receive_pack=false` is mandatory — omitting it refuses startup |
| Git LFS | `/info/lfs` + `/api/v1/lfs` | mounted in both modes; write authorization follows `push_auth` |
| Product API | `/api/v1/*` (status, file/blob, file/tree, preview reads + create-entry/delete-entry/move-entry/edit/save writes + tags) | trunk / storage-only mode |
| OCI registry | `/v2/` | `[oci].enabled` and storage-only; fail-closed (not registered) under review |
| Agent Capture | `/api/v1/agent-capture` | `[agent_capture].enabled` and storage-only |
| MST/2 snapshot surface | `/api/v2` | nest is static; handlers fail closed unless `[mst2].enabled`; toggling needs a restart |
| Swagger UI / OpenAPI | `/swagger-ui`, `/api/openapi.json` | always mounted; document content varies by mode |

Ports and container deployment parameters are covered in the [Deployment Guide](./deployment.md); the repo-root `Dockerfile` builds the release image.

For user-facing API and optional service behavior, see the [User Guide](./user-guide.md).

## 7. Configuration and hot reload

The config pipeline lives in `src/config/` (**not** `src/common/config`). Key points:

- **Load order**: `--config` → env `MEGA_CONFIG` → `./config/config.toml` → `$MEGA_BASE_DIR/etc/config.toml` → generated default. A profile (`--profile` / `MEGA_PROFILE`) is a sibling file `config.<profile>.toml` next to the main config, merged before env overrides.
- **Overrides and strictness**: env override pattern `MEGA_<SECTION>__<KEY>`; unknown fields are strictly rejected (leftover keys are errors, not silently ignored). The authoritative list of all config keys is the heavily commented sample [`../config/config.toml`](../config/config.toml); this document does not copy them.
- **Validation timing**: `Config::validate` runs on both the `AppContext::new` and `config validate` paths — service startup and CLI validation apply the same rules.
- **Hot reload**: the service command starts a 5s polling watcher (`CONFIG_RELOAD_POLL_INTERVAL`, `src/commands/service/mod.rs:22`); changes go through `ConfigHandle::reload`: whitelisted fields (`log`, `artifacts_gc`, `buck`, `notification`) apply live and notify subscribers, everything else is reported in `restart_required_fields` (`src/config/reload.rs:120`). Candidate configs pass `validate` before applying, and a failed apply can roll back.

See the [Configuration Guide](./configuration.md) for load order, SecretRef, and hot-reload behavior.

## 8. Further reading

- This documentation set: [`quick-start.md`](./quick-start.md) · [`user-guide.md`](./user-guide.md) · [`configuration.md`](./configuration.md) · [`deployment.md`](./deployment.md) · [`contributing.md`](./contributing.md)
- Repository paths, branches, tags, and push behavior: [`user-guide.md`](./user-guide.md)
- Deployment and operations: [`deployment.md`](./deployment.md); local development and testing: [`contributing.md`](./contributing.md)
- Error handling implementation: `crate::common::errors`; repository conventions: [`../AGENTS.md`](../AGENTS.md)
- Contribution flow and code conventions: [`contributing.md`](./contributing.md), [`../AGENTS.md`](../AGENTS.md)
