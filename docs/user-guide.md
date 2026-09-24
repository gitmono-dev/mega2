# User Guide

English · [中文](user-guide.zh.md)

Mega2 is the second-generation Mega engine built for Agents. Its core capabilities are the Monorepo engine and optional Agent Session Capture. The recommended Agent workflow combines Mega2 with ScorpioFS for local filesystem mounts and Libra for version-control workflows. This guide covers everyday Git, HTTP API, large-file, optional-service, and CLI operations. For deployment instructions, see the [Deployment Guide](./deployment.md); the commented [`config.toml`](../config/config.toml) is the authoritative list of configuration keys.

> **Scope:** the open-source edition supports only **trunk / storage-only** mode. It has no Web UI or Change List (CL). Use Libra's `libra mega2 browser` for interactive browsing and directory or tag operations (see §6).

## 1. Product shape and boundaries

Repository behavior depends on the path you use:

- **Monorepo paths** (the root path and paths outside `import_dir`) expose only the public branch `main`. Git-client tag writes and pushes to other branches are rejected. Create, list, and delete tags through the HTTP API (§4.2) or Libra's `libra mega2 browser` command (§6).
- **ImportRepo paths** under `[monorepo].import_dir` (default `/third-party`) follow ordinary Git semantics: they allow multiple branches and Git-client tag operations. Use them when migrating or hosting third-party repositories.
- The shipped storage-only service has no Web UI or Change List (CL). Its OpenAPI document omits CL, issue, reviewer, and user routes.
- Git pushes and product API writes share the same write queue and path-tip authority. The [architecture guide](./architecture.md) describes the write path.

## 2. Git client operations

### 2.1 Smart HTTP: clone / fetch / push

Git smart HTTP (`info/refs`, `git-upload-pack`, `git-receive-pack`) is mounted under the repository path and supports sub-path clones. See the [Deployment Guide](./deployment.md) for local Compose endpoints:

```bash
git clone http://127.0.0.1:9000/project
git fetch && git reset --hard origin/main   # client alignment after a trunk push
```

Push behavior in trunk mode:

- Push to `refs/heads/main`; pushes to other branches and Git-client tag writes are rejected on Monorepo paths.
- A single-commit push lands unchanged. A push containing multiple commits is squashed into one commit on `main`. Afterward, run `git fetch && git reset --hard origin/main`; otherwise, the next push may be rejected as non-fast-forward.
- Push to a repository subpath. Pushing from the root path `/` is unsupported in storage-only mode.

The protocol endpoints and write surfaces are listed in the [Architecture Guide](./architecture.md).

### 2.2 SSH: read-only fetch

storage-only **does not expose SSH receive-pack** (`git.ssh_receive_pack=false` is mandatory configuration; omitting it refuses startup). SSH is only for clone, fetch, and pull. See the [Deployment Guide](./deployment.md) for supported authentication modes.

### 2.3 Push auth: token or none

The write surfaces (Git receive-pack, LFS batch/lock writes, product API writes) share `git.push_auth`:

- `token`: HTTP Basic takes only the password (= the token secret); the username is ignored. `paths` prefixes authorize on component boundaries.
- `none`: credential-less writes, only for controlled networks (loopback / Unix socket / intranet front).

See the [Deployment Guide](./deployment.md) for token setup, credential injection, and security guidance; this guide does not reproduce token values.

### 2.4 Object format

`[monorepo].object_format` defaults to `sha1`, which works with standard Git clients. The `sha256` and `blake3` formats are Libra extensions and require the Libra client; standard Git interoperability is not supported for those formats. Configuration options are listed in [`config.toml`](../config/config.toml).

### 2.5 Monorepo path policy and first use

This section is the single authoritative description of creating a new path in the monorepo; the Quick Start, the README and the initialization manual link here.

**Allowed roots.** The first-level directories listed in `[monorepo].root_dirs` (values in [`config/config.toml`](../config/config.toml), shape rules in the [Configuration Guide](./configuration.md)) are the allow-list for creating paths: a new path can only be created under one of them. Paths under `import_dir` (default `/third-party`) are ImportRepos (see §1); a Git push creates them directly and this section does not apply. Adding a first-level root requires a config change and a restart; the new root is then still missing from the root tree, and only a push that adds commits on top of existing history can create it, because provisioning and product writes never land at `/`.

**First push to a new path.** If the path already exists (it was pushed, provisioned or written through the API), push as described in §2.1. If it does not exist yet, trunk mode handles the push as follows; both path-policy rejections in the table stay the same on a verbatim retry:

| Case | Result | Next step |
|---|---|---|
| The path is not under any allowed root | Rejected with `MONO_PATH_NOT_ALLOWED`, listing the allowed roots | Use a path under one of the roots; for a new root see "Allowed roots" above |
| The path is under an allowed root and the pushed history starts from nothing (e.g. the first commit after `git init`), or is another path's history pushed unchanged | Rejected with `MONO_PATH_UNINITIALIZED`, naming the provisioning command | Provision the path as below, clone it, commit on top and push; for a new root that is not in the root tree yet see "Allowed roots" above |
| The path is under an allowed root and the push adds at least one commit on top of a commit the server already has (e.g. committing in a clone of another path) | Accepted and created | After the push, run `git fetch && git reset --hard origin/main` to align |

In the second and third cases the new path advertises no refs, so Git sends the whole source history: if it carries more than `[monorepo].max_push_commits` (default 250) commits, the push is rejected with that limit (no path-policy code; a verbatim retry may show a different text). Provision the path instead under an existing root; a root added after initialization cannot be provisioned, so clone a path with a short history, commit on top and push that to the new root.

**Provisioning a path.** Provisioning only creates the directory (one `.gitkeep` commit) and is safe to repeat:

- CLI: `mega2 path provision --server <URL> <PATH>`. It reads the push token only from the environment variable `MEGA2_TOKEN` (omit it under `push_auth=none`) and reads no config file. On success it prints `created <path> (<commit>)` or `already exists <path>` and exits 0; when the server refuses, it writes the error text to stderr and exits 1.
- HTTP: `POST /api/v1/path/provision` with body `{"path": "<PATH>"}`; authentication, status codes and the response shape are in the runtime OpenAPI (§4.4) and in the contract page [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) (Chinese).

After provisioning, `git clone <URL><PATH>`, commit on top and push. Product writes (§4.1) under an allowed root that is already in the root tree create the path themselves and need no provisioning.

**Error codes.** Path-policy error texts start with a stable code, so clients can branch on the code before the colon; the exact text format, HTTP status and where each code occurs are in the [error model](./errors.md) (Chinese). The four codes and what to do:

- `MONO_PATH_NOT_ALLOWED`: use a path under an allowed root (provisioning and product writes also return it for paths under `import_dir`).
- `MONO_PATH_UNINITIALIZED`: provision the path first.
- `MONO_PATH_INVALID`: use a canonical path (absolute, no `.` / `..`, repeated or trailing `/`).
- `MONO_PATH_CONFLICT`: a component of the path is a file; choose another path (returned only by provisioning, through the API or the CLI).

## 3. Large files: LFS

- **Git LFS (standard)**: endpoints `/info/lfs` and `/api/v1/lfs` work with standard `git-lfs` clients. LFS writes use the same `git.push_auth` setting as Git receive-pack; see the [Deployment Guide](./deployment.md) for authentication modes.

## 4. HTTP API usage

The base URL is the HTTP service listen address. Write endpoints authenticate via `git.push_auth`: missing/bad credentials get 401, path-scope violations get 403 (same as the Git write surface).

### 4.1 Product writes (directories and files)

`POST /api/v1/create-entry`, `POST /api/v1/delete-entry`, `POST /api/v1/move-entry`, and `POST /api/v1/edit/save` write files and directories. The path tip advances through the same write queue used by `git push`. A successful response carries `cl_link: null` (no CL is created), and a same-stack `git clone` or `git pull` can read the committed content immediately. Index-backed browse results may take a short time to catch up after a write. The runtime OpenAPI document describes request fields and response schemas.

### 4.2 Tags API

The only entry point now that Git-client tags are forbidden:

| Operation | HTTP |
|---|---|
| Create | `POST /api/v1/tags` |
| List | `GET /api/v1/tags/list` (GET-only; `POST` returns 405) |
| Get | `GET /api/v1/tags/{name}` |
| Delete | `DELETE /api/v1/tags/{name}` |

Create and delete requests authenticate through `git.push_auth`; list and get requests do not require authorization. The runtime OpenAPI document describes request fields and response schemas.

### 4.3 Read-only browsing

`GET /api/v1/status`, `GET /api/v1/file/blob/{object_id}`, `GET /api/v1/file/tree`, and the blob / tree / blame preview read paths remain available; read authorization follows `git.anonymous_access`. The full path list is the runtime OpenAPI (see §4.4); this guide does not duplicate the endpoint table.

### 4.4 OpenAPI and Swagger UI

- Machine-readable contract: `GET /api/openapi.json` (under storage-only it truthfully omits CL / issue / user routes).
- Interactive docs: `/swagger-ui`.

### 4.5 Error contract

Error types and HTTP status mapping are centralized in `crate::common::errors`.

## 5. Optional service surfaces

Both surfaces require storage-only mode and their own config switch. If either condition is missing, the route is not mounted and returns 404; enabling either switch outside storage-only mode prevents startup.

- **OCI Distribution `/v2`**: mounted when storage-only and `[oci].enabled=true`; serves as a container registry (`docker login` reuses push tokens — there is no separate token service). See the [Deployment Guide](./deployment.md) for enablement and usage.
- **Agent Session Capture `/api/v1/agent-capture`**: an optional API for ingesting and querying Agent sessions, events, checkpoints, transcripts, and file operations. It is mounted only in storage-only mode when `[agent_capture].enabled=true` and at least one `[[agent_capture.ingest_tokens]]` is configured. It uses separate ingest tokens and is independent of Git push; a Git push does not create a session capture record. See the [configuration and API reference](./refactoring/agent-capture.md).

## 6. Using mega2 with Libra

The open-source edition has no Web UI. Libra's `libra mega2 browser` command provides basic remote directory browsing and supported directory and tag operations. These operations do not replace Git client clone, fetch, or push:

```bash
libra mega2 browser
```

The TUI browses remote directories one level at a time and supports creating, deleting, moving, and renaming directories. Its Tag panel lists, creates, and deletes root tags; creating or deleting requires write credentials. It reads remote directory and tag data from Mega2 and does not replace a Git client's clone, fetch, or push. The sha256 / blake3 object formats are likewise only available with the Libra client (see §2.4).

## 7. CLI quick reference

Global flags: `--config <PATH>` (env `MEGA_CONFIG`) and `--profile <NAME>` (env `MEGA_PROFILE`, loading the sibling `config.<profile>.toml`). See the [Configuration Guide](./configuration.md) for load order, environment overrides (`MEGA_<SECTION>__<KEY>`), unknown-field rejection, and hot reload. For a new database, `mega2 service init --yes` creates the initial Monorepo from the configured `root_dirs` and exits without starting listeners; the commented [`config.toml`](../config/config.toml) shows the directory defaults.

| Command | Purpose |
|---|---|
| `mega2 service init --yes` | Initialize an empty Monorepo and exit (no listeners started); the fixed bootstrap action for an empty volume |
| `mega2 service http --host 0.0.0.0 -p 8000` | Start the HTTP surface (Git smart HTTP + LFS + `/api/v1` + optional OCI / Agent Capture); matches the Dockerfile default CMD |
| `mega2 service ssh [--ssh-port 2222]` | Start the SSH surface (upload-pack only under storage-only); standalone requires `cedar.enforcement=off`, otherwise use `multi` |
| `mega2 service multi http ssh` | Start HTTP + SSH in one process (shared authorization snapshot) |
| `mega2 path provision --server <URL> <PATH>` | Provision a monorepo path (before its first push, see §2.5); the token comes only from the environment variable `MEGA2_TOKEN`, no config file is read |
| `mega2 config validate [--resolve-secrets] [--show-sources]` | Validate configuration before startup; add `--deny-warnings` in automation |
| `mega2 config init [-o PATH] [--force]` | Generate a safe starter configuration file |
| `mega2 config secret ref/set/check/rotate` | Manage vault-backed config secrets (SecretRef) |
| `mega2 config vault backup/restore/rekey/reset` | Vault core-key operations (the last three are destructive and require `--force`) |
| `mega2 debug storage-smoke [--key ...]` | Hidden command: object-storage read / write / delete smoke; absent from top-level help |

The `authz-audit` subcommand audits authorization in review mode. Under storage-only, `cedar.enforcement` is always `off`, so this command is not part of day-to-day use. The [Architecture Guide](./architecture.md) summarizes the authorization modes.

The full flag list lives in `--help` and `src/commands/`. See the [Deployment Guide](./deployment.md) for Compose setup and the [Contributing Guide](./contributing.md) for the development workflow and required checks.

## 8. Related documents

| Topic | Document |
|---|---|
| Guides (quick start / recipes / config / deployment / architecture / contributing) | [`quick-start.md`](./quick-start.md) · [`recipes.md`](./recipes.md) · [`configuration.md`](./configuration.md) · [`deployment.md`](./deployment.md) · [`architecture.md`](./architecture.md) · [`contributing.md`](./contributing.md) |
| Repository paths, branches, tags, and push behavior | Sections 1–2 of this guide |
| Trunk / storage-only deployment and the Compose stack | [`deployment.md`](./deployment.md) |
| Config keys and environment variables | [`config.toml`](../config/config.toml) (commented sample), [`configuration.md`](./configuration.md) |
| Git protocol surfaces and write path | [`architecture.md`](./architecture.md) |
| Directory, file, and tag operations | Sections 4.1–4.2 of this guide |
| OCI registry and Agent Capture routes | [`architecture.md`](./architecture.md) and [`deployment.md`](./deployment.md) |
| Error handling implementation | `crate::common::errors` |
| Contribution workflow and required checks | [`contributing.md`](./contributing.md) |
