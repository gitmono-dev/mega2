# User Guide

English · [中文](user-guide.zh.md)

This is the **user guide** for mega2, covering the surfaces you touch day to day: Git clients, the HTTP API, large files, optional service surfaces, and the CLI. It only orients and shows minimal examples; fact values (config keys, tokens, ports, endpoint lists) live in their authoritative docs — product rules in [`monorepo.md`](./monorepo.md), deployment and ops in [`deploy-trunk.md`](./deploy-trunk.md), the configuration contract in [`refactoring/config.md`](./refactoring/config.md) plus the heavily commented sample [`config/config.toml`](../config/config.toml), and the error model in [`errors.md`](./errors.md).

> **Scope**: the mega2 open-source edition ships only the **trunk / storage-only** shape — no Web UI, no Change List (CL). Interactive browsing and directory / tag operations go through Libra's `libra mega2 browser` (see §6).

## 1. Product shape and boundaries

Core rules (source of truth: [`monorepo.md`](./monorepo.md)):

- **Single public branch `main`**: pushing any public head other than `refs/heads/main` is rejected at the protocol layer; `refs/cl/*` belongs to the CL pipeline and is not part of the storage-only delivery.
- **Git-client tags are forbidden**: `git push --tags` and any other client-side tag write exits non-zero and leaves no residue on the remote; tag creation / query / deletion go through the HTTP API only (see §4.2).
- **ImportRepo exception**: repositories under `[monorepo].import_dir` (default `/third-party`) follow ordinary Git semantics — multi-branch and client tags are legal there, for hosting third-party dependency sources.
- **No CL / no Web UI**: storage-only does not register CL / issue / reviewer / OAuth user routes; the OpenAPI document reflects this (they are absent).
- **Unified write authority**: `git push` and the product write APIs share `MonoWriteQueue` to advance path tips (design: [`refactoring/trunk-push.md`](./refactoring/trunk-push.md)); root-tree writes are globally serialized.

## 2. Git client operations

### 2.1 Smart HTTP: clone / fetch / push

Git smart HTTP (`info/refs`, `git-upload-pack`, `git-receive-pack`) is mounted under the repository path and supports sub-path clones (local compose stack ports: [`deploy-trunk.md`](./deploy-trunk.md) §8):

```bash
git clone http://127.0.0.1:9000/project
git fetch && git reset --hard origin/main   # client alignment after a trunk push
```

Push landing rules (trunk shape):

- Only `refs/heads/main` may be pushed; other heads and tags are rejected.
- An N = 1 (single-commit) push lands as-is; an N > 1 chained push is squashed by the server into one commit advancing `main`, after which you must run `git fetch && git reset --hard origin/main` to align — otherwise the next push is rejected as non-fast-forward (N-split details: [`monorepo.md`](./monorepo.md) §9; agent workflow memo: [`deploy-trunk.md`](./deploy-trunk.md) §9).
- Sub-path pushes are the norm; pushing the root path `/` is not an assumption of this shape.

Protocol compatibility and the smoke matrix: [`refactoring/protocol.md`](./refactoring/protocol.md).

### 2.2 SSH: read-only fetch

storage-only **does not expose SSH receive-pack** (`git.ssh_receive_pack=false` is mandatory configuration; omitting it refuses startup). SSH is only for clone / fetch / pull. The authentication forms under each `anonymous_access` / `push_auth` combination: [`deploy-trunk.md`](./deploy-trunk.md) §4.

### 2.3 Push auth: token or none

The write surfaces (Git receive-pack, LFS batch/lock writes, product API writes) share `git.push_auth`:

- `token`: HTTP Basic takes only the password (= the token secret); the username is ignored. `paths` prefixes authorize on component boundaries.
- `none`: credential-less writes, only for controlled networks (loopback / Unix socket / intranet front).

`[[git.push_tokens]]` configuration, credential injection, and risk warnings: [`deploy-trunk.md`](./deploy-trunk.md) §3; this guide does not copy token values.

### 2.4 Object format

`[monorepo].object_format` defaults to `sha1` (stock Git). `sha256` and `blake3` are **git-internal / Libra extensions** with no claimed interoperability with stock Git clients; they require Libra. See [`refactoring/protocol.md`](./refactoring/protocol.md) and [`refactoring/config.md`](./refactoring/config.md).

## 3. Large files: LFS and FastCDC Media

- **Git LFS (standard)**: endpoints `/info/lfs` and `/api/v1/lfs`, usable directly by stock `git-lfs` clients; LFS write authorization shares `git.push_auth` with Git receive-pack (matrix: [`deploy-trunk.md`](./deploy-trunk.md) §6).
- **FastCDC Media (Libra extension)**: large media is uploaded and reused as content-defined chunks; it requires a server built with `--features fastcdc` and Libra as the client. It is **not** a standard Git LFS extension and claims no stock-Git interoperability. Protocol contract: [`refactoring/fastcdc-media.md`](./refactoring/fastcdc-media.md).

## 4. HTTP API usage

The base URL is the HTTP service listen address. Write endpoints authenticate via `git.push_auth`: missing/bad credentials get 401, path-scope violations get 403 (same as the Git write surface).

### 4.1 Product writes (directories and files)

`POST /api/v1/create-entry`, `POST /api/v1/delete-entry`, `POST /api/v1/move-entry`, `POST /api/v1/edit/save`. Objects are written to storage and the path tip is advanced through MonoWriteQueue — the same tip authority as `git push`. A successful response carries `cl_link: null` (no CL is created), and a same-stack `git clone` / `git pull` sees the new content immediately. Request fields and the authorization contract: [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md).

### 4.2 Tags API

The only entry point now that Git-client tags are forbidden:

| Operation | HTTP |
|---|---|
| Create | `POST /api/v1/tags` |
| List | `GET /api/v1/tags/list` (GET-only; `POST` returns 405) |
| Get | `GET /api/v1/tags/{name}` |
| Delete | `DELETE /api/v1/tags/{name}` |

create / delete authenticate via `git.push_auth`; list / get do not require Authorization. Path selectors and authorization details: [`monorepo.md`](./monorepo.md) §2 and [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md).

### 4.3 Read-only browsing

`GET /api/v1/status`, `GET /api/v1/file/blob/{object_id}`, `GET /api/v1/file/tree`, and the blob / tree / blame preview read paths remain available; read authorization follows `git.anonymous_access`. The full path list is the runtime OpenAPI (see §4.4); this guide does not duplicate the endpoint table.

### 4.4 OpenAPI and Swagger UI

- Machine-readable contract: `GET /api/openapi.json` (under storage-only it truthfully omits CL / issue / user routes).
- Interactive docs: `/swagger-ui`.

### 4.5 Error contract

Error type ownership and HTTP status mapping are centralized in `crate::common::errors`; the rules: [`errors.md`](./errors.md).

## 5. Optional service surfaces

Both surfaces are **dual-gated** (storage-only shape + their own switch); with either condition missing the whole surface is absent (bare 404), and enabling the switch outside storage-only refuses startup.

- **OCI Distribution `/v2`**: mounted when storage-only and `[oci].enabled=true`; serves as a container registry (`docker login` reuses push tokens — there is no separate token service). Architecture and endpoint source of truth: [`refactoring/oci.md`](./refactoring/oci.md); enablement steps: [`deploy-trunk.md`](./deploy-trunk.md) §10.
- **Agent Capture `/api/v1/agent-capture`**: mounted when storage-only and `[agent_capture].enabled=true`; captures agent sessions / events / checkpoints / file operations, with its own `[[agent_capture.ingest_tokens]]` authentication surface. Configuration and quota source of truth: [`refactoring/agent-capture.md`](./refactoring/agent-capture.md).

## 6. Using mega2 with Libra

mega2 itself serves no interactive interface. Day-to-day browsing and directory / tag operations run inside a Libra working copy:

```bash
libra mega2 browser
```

That TUI consumes the HTTP surface of §4 (tree reads, create / delete / move-entry, tags). The sha256 / blake3 object formats and FastCDC Media are likewise only available with the Libra client (see §2.4 and §3).

## 7. CLI quick reference

Global flags: `--config <PATH>` (env `MEGA_CONFIG`) and `--profile <NAME>` (env `MEGA_PROFILE`, loading the sibling `config.<profile>.toml`). Config load order, environment overrides (`MEGA_<SECTION>__<KEY>`), unknown-field rejection, and the hot-reload whitelist: [`refactoring/config.md`](./refactoring/config.md).

| Command | Purpose |
|---|---|
| `mega2 service init --yes` | Initialize an empty Monorepo and exit (no listeners started); the fixed bootstrap action for an empty volume |
| `mega2 service http --host 0.0.0.0 -p 8000` | Start the HTTP surface (Git smart HTTP + LFS + `/api/v1` + optional OCI / Agent Capture); matches the Dockerfile default CMD |
| `mega2 service ssh [--ssh-port 2222]` | Start the SSH surface (upload-pack only under storage-only); standalone requires `cedar.enforcement=off`, otherwise use `multi` |
| `mega2 service multi http ssh` | Start HTTP + SSH in one process (shared authorization snapshot) |
| `mega2 config validate [--resolve-secrets] [--show-sources]` | Validate configuration before startup; add `--deny-warnings` in automation |
| `mega2 config init [-o PATH] [--force]` | Generate a safe starter configuration file |
| `mega2 config secret ref/set/check/rotate` | Manage vault-backed config secrets (SecretRef) |
| `mega2 config vault backup/restore/rekey/reset` | Vault core-key operations (the last three are destructive and require `--force`) |
| `mega2 debug storage-smoke [--key ...]` | Hidden command: object-storage read / write / delete smoke; absent from top-level help |

The `authz-audit` subcommand targets authorization auditing in the review morphology; under storage-only `cedar.enforcement` is always `off`, so it is not part of daily use (review-morphology coverage: [`manual/authz.md`](./manual/authz.md)).

The full flag list lives in `--help` and `src/commands/`. Compose bootstrap and smoke command samples: [`deploy-trunk.md`](./deploy-trunk.md) §8; local development and test entry points: [`development.md`](./development.md).

## 8. Related documents

| Topic | Document |
|---|---|
| This documentation set (quick start / config / deployment / architecture / contributing) | [`quick-start.md`](./quick-start.md) · [`configuration.md`](./configuration.md) · [`deployment.md`](./deployment.md) · [`architecture.md`](./architecture.md) · [`contributing.md`](./contributing.md) |
| Product rules (single branch, tag ban, ImportRepo, trunk invariants) | [`monorepo.md`](./monorepo.md) |
| Trunk / storage-only deployment and the compose stack | [`deploy-trunk.md`](./deploy-trunk.md) |
| Config keys and environment variables | [`config/config.toml`](../config/config.toml) (commented sample), [`refactoring/config.md`](./refactoring/config.md) |
| Git protocol compatibility | [`refactoring/protocol.md`](./refactoring/protocol.md) |
| Trunk push design and MonoWriteQueue | [`refactoring/trunk-push.md`](./refactoring/trunk-push.md) |
| Directory / file write and tag contract | [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) |
| FastCDC Media | [`refactoring/fastcdc-media.md`](./refactoring/fastcdc-media.md) |
| OCI registry | [`refactoring/oci.md`](./refactoring/oci.md) |
| Agent Capture | [`refactoring/agent-capture.md`](./refactoring/agent-capture.md) |
| Error model | [`errors.md`](./errors.md) |
| Monorepo initialization products | [`manual/monorepo-init.md`](./manual/monorepo-init.md) |
| Authentication / authorization boundary (review morphology) | [`manual/authz.md`](./manual/authz.md) |
| Local development and tests | [`development.md`](./development.md) |
