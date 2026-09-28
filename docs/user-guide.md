# User Guide

English · [中文](user-guide.zh.md)

Mega2 is the second-generation Mega engine built for Agents. Its core capabilities are the Monorepo engine and optional Agent Session Capture. The recommended Agent workflow combines Mega2 with ScorpioFS for local filesystem mounts and Libra for version-control workflows. This guide covers everyday Git, HTTP API, large-file, optional-service, and CLI operations. For deployment instructions, see the [Deployment Guide](./deployment.md); the commented [`config.toml`](../config/config.toml) is the authoritative list of configuration keys.

> **Scope:** the open-source edition supports only **trunk / storage-only** mode. It has no Web UI or Change List (CL). Use Libra's `libra mega2 browser` for interactive browsing and directory or tag operations (see §6).

## 1. Product shape and boundaries

Repository behavior depends on the path you use:

- **Monorepo paths** (the root path and paths outside `import_dir`) expose only the public branch `main`. Git-client tag writes and pushes to other branches are rejected. Create, list, and delete tags through the HTTP API (§4.2) or Libra's `libra mega2 browser` command (§6).
- **ImportRepo paths** under `[monorepo].import_dir` (default `/third-party`) follow ordinary Git semantics: they allow multiple branches and Git-client tag operations, accept later pushes, and can be removed (§2.6). Use them when migrating or hosting third-party repositories.
- The shipped storage-only service has no Web UI or Change List (CL). Its OpenAPI document omits CL, issue, reviewer, and user routes.
- On Monorepo paths, Git pushes and product API writes share the same write queue and path-tip authority; edit/save inside a live ImportRepo writes that repository directly, see §4.1. The [architecture guide](./architecture.md) describes the write path.

## 2. Git client operations

### 2.1 Smart HTTP: clone / fetch / push

Git smart HTTP (`info/refs`, `git-upload-pack`, `git-receive-pack`) is mounted under the repository path and supports sub-path clones. See the [Deployment Guide](./deployment.md) for local Compose endpoints:

```bash
git clone http://127.0.0.1:9000/project
git fetch && git reset --hard origin/main   # client alignment after a trunk push
```

Push behavior in trunk mode (Monorepo paths; for ImportRepos see §2.6):

- Push to `refs/heads/main`; pushes to other branches and Git-client tag writes are rejected on Monorepo paths.
- A single-commit push lands unchanged. A push containing multiple commits is squashed into one commit on `main`. Afterward, run `git fetch && git reset --hard origin/main`; otherwise, the next push may be rejected as non-fast-forward.
- Push to a repository subpath. Pushing from the root path `/` is unsupported in storage-only mode.

The protocol endpoints and write surfaces are listed in the [Architecture Guide](./architecture.md).

### 2.2 SSH: read-only fetch

storage-only **does not expose SSH receive-pack** (`git.ssh_receive_pack=false` is mandatory configuration; omitting it refuses startup). SSH is only for clone, fetch, and pull. See the [Deployment Guide](./deployment.md) for supported authentication modes.

### 2.3 Push auth: token or none

The write surfaces (Git receive-pack, LFS batch/lock writes, product API writes) share `git.push_auth`:

- `token`: HTTP Basic takes only the password (= the token secret); the username is ignored. `paths` prefixes authorize on component boundaries.
- `none`: credential-less writes, only for controlled networks (loopback / Unix socket / intranet front); the HTTP ImportRepo removal endpoint is the exception and answers 403 to every valid request under `none` (see §2.6).

See the [Deployment Guide](./deployment.md) for token setup, credential injection, and security guidance; this guide does not reproduce token values.

### 2.4 Object format

`[monorepo].object_format` defaults to `sha1`, which works with standard Git clients. The `sha256` and `blake3` formats are Libra extensions and require the Libra client; standard Git interoperability is not supported for those formats. Configuration options are listed in [`config.toml`](../config/config.toml).

### 2.5 Monorepo path policy and first use

This section is the single authoritative description of creating a new path in the monorepo; the Quick Start, the README and the initialization manual link here.

**Allowed roots.** The first-level directories listed in `[monorepo].root_dirs` (values in [`config/config.toml`](../config/config.toml), shape rules in the [Configuration Guide](./configuration.md)) are the allow-list for creating paths: a new path can only be created under one of them. Paths under `import_dir` (default `/third-party`) are ImportRepos (see §1); a Git push creates them directly and this section does not apply; §2.6 covers their updates and removal. Adding a first-level root requires a config change and a restart; the new root is then still missing from the root tree, and only a push that adds commits on top of existing history can create it, because provisioning and product writes never land at `/`.

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

- `MONO_PATH_NOT_ALLOWED`: use a path under an allowed root (provisioning, and product writes where no ImportRepo is live, also return it for paths under `import_dir`).
- `MONO_PATH_UNINITIALIZED`: provision the path first.
- `MONO_PATH_INVALID`: use a canonical path (absolute, no `.` / `..`, repeated or trailing `/`).
- `MONO_PATH_CONFLICT`: a component of the path is a file; choose another path (returned only by provisioning, through the API or the CLI).

### 2.6 ImportRepo lifecycle

This section is the authoritative description of how ImportRepos are updated, removed and retained, including the operator CLI `mega2 import-repo remove`. ImportRepos live under `[monorepo].import_dir` (default `/third-party`); the [initialization manual](./manual/monorepo-init.md) explains how `import_dir` classifies paths.

**Creation and later pushes.** A push to a path under `import_dir` that does not exist yet creates an ImportRepo and mounts it in the Monorepo root tree. The path must lie strictly below `import_dir`: the server rejects a push to `import_dir` itself with a 400 `IMPORT_REPO_PATH_INVALID`, which the Git client shows only as `The requested URL returned error: 400`. From then on the ImportRepo is an ordinary Git repository that you can keep updating: creating, updating and deleting branches and tags, and `--force` non-fast-forward updates, are all accepted. Later pushes only update refs and add no commit to the Monorepo root tree, so the squash and the `git fetch && git reset --hard origin/main` alignment step of §2.1 do not apply to ImportRepos.

**Concurrency and failure granularity.** Every ref update in a push is conditional on the value the server advertised: if another push or API write changed the ref after the advertisement, that update is rejected with `IMPORT_REPO_STALE_REF` (more often, the Git client rejects the push with `fetch first` before sending it): get the ImportRepo's latest refs (for example `git fetch mega2`), merge or rebase, and push again, or use `--force` when you mean to overwrite; a conflicting tag has to be resolved on its own. The product API's `edit/save` writes the default branch directly without this check and can overwrite a concurrent push (§4.1). Within one push, tags are written one by one and succeed or fail independently; all branches are written as one batch, and if the batch fails every branch reports the same reason and none of them moves. The push report always starts with `unpack ok`; the server does not support `git push --atomic`. The full text, HTTP status and occurrences of each error code are in the "ImportRepoError" section of the [error model](./errors.md) (Chinese).

**Nested ImportRepos.** Pushing a parent path first and then a child path below it works, and the parent can still be updated afterwards (except when the parent is a legacy mount from before the upgrade with no mount-provenance record, see "Compatibility" below). In the reverse order, when a child already exists, the parent's branches are rejected with `IMPORT_REPO_PATH_OCCUPIED`. Children must be removed before their parent; otherwise removal is rejected with `IMPORT_REPO_HAS_CHILDREN`.

**Removal.** Removal detaches the ImportRepo from the service: it deletes the repository record and its refs, removes the mount this repository owns from the Monorepo root tree (with a root commit `Remove ImportRepo <P>`; mount content that is not its own stays, see "Compatibility" below), and then sweeps the repository's commit, tree, blob and tag metadata in batches. After the detach, clone and fetch of the path return 404. For a push still in progress when the detach commits, the refs it has yet to write are rejected with `IMPORT_REPO_REMOVED` and its branches do not land (tags are written one by one; a tag written before the detach belongs to the old repository and is swept with it); a push that starts after the detach — even while the sweep is unfinished — imports the path as a new repository. There are two entry points that use the same cleanup mechanism:

- HTTP `POST /api/v1/import-repo/remove` (§4.6): requires `git.push_auth = "token"` and a token that covers the path (§2.3); deployments with `push_auth = "none"` get 403 for every valid removal request (a malformed body or path is answered first with 400 / 422). Request fields, outcomes (`removed` / `pending` / `absent`), the continuation protocol and error codes are in the "ImportRepo 叶子清理" section of the contract page [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) (Chinese).
- The operator CLI `mega2 import-repo remove`: runs on the server with the service's own config, for deployments such as `push_auth = "none"` that cannot use HTTP removal; being able to read the service's config and the database credentials in it is the authorization.

For both entry points `absent` means the path has no unfinished cleanup left, and so does the CLI's `removed` (a run drains the path's other cleanups before it finishes); the HTTP `removed` only guarantees that the cleanup the request refers to is complete, and older unfinished cleanups of the same path may remain for later requests (see the contract page).

**Operator CLI.** On a Compose stack, run it inside the mega2 container (the image sets `MEGA_CONFIG`; use the same `-f` / `-p` as when the stack was started). On a bare-metal deployment, use the service's config file, `--profile` / `MEGA_PROFILE` and `MEGA_*` environment variables (for example a database URL injected through the environment), and put `--config` before `import-repo`:

```bash
docker compose -f <compose file> [-p <project>] exec -T -e MEGA_LOG__PRINT_STD=false mega2 mega2 import-repo remove --path /third-party/<repo> --yes
mega2 --config /etc/mega2/config.toml import-repo remove --path /third-party/<repo> --yes
```

- Prerequisites: version 0.40.11 or later; the service is running (it reclaims write-queue rows an interrupted run leaves behind); the same image or binary as the service — a binary whose schema does not match the database is refused, and the command never migrates the database. Only storage-only deployments are accepted (`git.push_auth` is `token` or `none`). The config must be named with `--config` or `MEGA_CONFIG`; no default config is generated. For this run, `redis.url` and the S3 credentials must be literal values (`vault://` references are refused; override them with `MEGA_REDIS__URL`, `MEGA_OBJECT_STORAGE__S3__ACCESS_KEY_ID` and `MEGA_OBJECT_STORAGE__S3__SECRET_ACCESS_KEY`); the command never opens the vault. Do not set `MEGA_ID_GENERATOR_WORKER_ID` for this run: with it set, the CLI no longer claims a worker lease from Redis and may use the same worker id as the service. Without `--yes`, nothing is done.
- Output: the last line of stdout is the outcome line, with control characters in the path escaped; when logs go to stdout (`MEGA_LOG__PRINT_STD=true`), the lines before it are logs. With exit codes 1 and 2 there is no outcome line and stderr gives the reason; with exit codes 3 and 130, stdout has the outcome line and stderr explains it.
- Ctrl-C: the 130 rows below apply when the CLI runs in the foreground directly. Under `docker compose exec -T`, Ctrl-C only ends the Compose client (which exits 130 with no outcome line) while the command in the container keeps running to the end: do not rerun right away; confirm it has finished as described below, then check the ledger.

| Outcome line / situation | Exit code | Meaning and next step |
|---|---|---|
| `removed <P> (repo_id=R, cleanup_id=C)` | 0 | Detached and fully swept. When the path has no live repository and only unfinished cleanups remain, R and C can refer to an earlier removed repository |
| `absent <P>` | 0 | This path has neither a live repository nor an unfinished cleanup. `--path` matches the stored repository path exactly as given: it does not strip `.git` (a Git URL does) and does not correct spelling, so before running, look it up with the `psql` form under "Checking the ledger" below, `SELECT id, repo_path FROM git_repo WHERE repo_path = '<P>'`, and confirm the path and repository id |
| `pending <P> (repo_id=R, cleanup_id=C)` | 3 | Detached, sweep not finished (`--max-rounds` reached, 1000 rounds by default, or a failure after the detach); continue with `--cleanup-id C` |
| `interrupted <P> (repo_id=R, cleanup_id=C)` | 130 | Interrupted with Ctrl-C after the detach committed; continue with `--cleanup-id C` |
| `interrupted <P>` | 130 | Interrupted with Ctrl-C before a cleanup was recorded; the repository is unchanged, rerun the same command; if stderr says the ledger could not be read, see below |
| stderr gives the error (for example `<CODE>: …`, `import-repo remove: …`, `Other error: …`, `Database error: …`) | 1 | No cleanup was recorded by this run: act on the error code (for example, remove the children first for `IMPORT_REPO_HAS_CHILDREN`) or fix the prerequisite and rerun; if the ledger could not be read, see below |
| Usage error such as a missing `--yes` | 2 | Nothing was done |

- Continuing: `--cleanup-id <C>` (with the same `--path`) only continues sweeping that ledger row and never detaches a live repository, so it is safe even after the path was pushed again; rerunning without `--cleanup-id` detaches whatever repository is live at that moment, including one imported in the meantime. Once the primary cleanup is complete, a run also sweeps the path's other unfinished cleanups from the ledger (with `--cleanup-id`, possibly newer rows too), again without detaching any repository; a run that ends `pending` may leave those rows untouched.
- Ledger could not be read: after a failure or an interrupt, the CLI reads the cleanup ledger to print a continuation handle; if stderr says the ledger could not be read (exit 1, or 130 with an outcome line without ids), the detach may have committed, so check the ledger as described next before continuing or rerunning.
- Checking the ledger: if the process was killed (SIGTERM, SIGHUP and SIGKILL are not handled), crashed, or got Ctrl-C under `exec -T`, first confirm the command has finished (on a Compose stack, `docker compose -f <compose file> [-p <project>] top mega2` shows whether `mega2 import-repo remove` is still running in the container), then query the path's cleanup ledger (the evaluation stack's database user and name are both `mega2`; other deployments use the database from the service's config):

  ```bash
  docker compose -f <compose file> [-p <project>] exec -T postgres psql -U mega2 -d mega2 -c "SELECT id, repo_id, state FROM import_repo_cleanups WHERE path = '/third-party/<repo>' ORDER BY id"
  ```

  Continue each row whose `state` is `detached` with `--cleanup-id <id>`; no `detached` row means no cleanup is unfinished. If the path still clones, it may be the original repository (the detach never committed) or a repository imported again in the meantime; check first (for example, its latest commits) before deciding to rerun, since a rerun detaches whichever repository is live.

**Retention.** Removal deletes metadata: the repository record and refs, the mount it owns in the Monorepo root tree, and the repository's commit, tree, blob and tag rows (the latter are swept in rounds, so a large repository can take several). Blob bytes in object storage are kept; the server has no garbage collection. The cleanup ledger `import_repo_cleanups` and the audit log (`kind = import_repo.remove`) are kept permanently and record the requester: the token name for HTTP, `operator-cli` for the CLI. Removal cannot be undone; pushing the same path again imports it as a new repository (a new `repo_id`). Known limitations are listed in the "ImportRepo 叶子清理" section of the contract page (Chinese).

**Compatibility.** Existing ImportRepos accept later pushes without any data migration: a legacy mount whose mount point holds only `.gitkeep` and whose repository has at least one branch gets its mount-provenance record filled in by the next push that creates or updates a branch. Two other legacy shapes reject pushes that create or update a branch with `IMPORT_REPO_PATH_OCCUPIED` (tag pushes and branch-delete-only pushes are not affected):

- A parent mounted before the upgrade that already has a nested child ImportRepo, in this order: (1) back up (`git clone --mirror`) and remove the children; (2) push a branch create or update to the parent, which records its mount (with nothing new to push, push a temporary branch and then delete it); (3) only then push the children back. Pushing a child back before step 2 gets the parent rejected again.
- A legacy mount without any branch: removal deletes only its records and leaves the placeholder directory in the root tree, so the path still cannot be imported afterwards; use another path. Until a legacy mount has its provenance record, do not delete all of its branches with a delete-only push, or it ends up in this shape.

The cleanup ledger table is created by the automatic database migration on upgrade; from 0.40.4, upgrades also normalize existing ImportRepo alias paths, see section 9.2 of [`deploy-trunk.md`](./deploy-trunk.md) (Chinese).

## 3. Large files: LFS

- **Git LFS (standard)**: endpoints `/info/lfs` and `/api/v1/lfs` work with standard `git-lfs` clients. LFS writes use the same `git.push_auth` setting as Git receive-pack; see the [Deployment Guide](./deployment.md) for authentication modes.

## 4. HTTP API usage

The base URL is the HTTP service listen address. Write endpoints authenticate via `git.push_auth`: missing/bad credentials get 401, path-scope violations get 403 (same as the Git write surface).

### 4.1 Product writes (directories and files)

`POST /api/v1/create-entry`, `POST /api/v1/delete-entry`, `POST /api/v1/move-entry`, and `POST /api/v1/edit/save` write files and directories. On Monorepo paths, the path tip advances through the same write queue used by `git push`. Product writes inside a live ImportRepo are handled by that repository: `edit/save` writes the default branch directly, without the write queue and without the ref check a push makes; delete and move return 409, and create is not supported (it currently returns 500). A successful response carries `cl_link: null` (no CL is created), and a same-stack `git clone` or `git pull` can read the committed content immediately. Index-backed browse results may take a short time to catch up after a write. The runtime OpenAPI document describes request fields and response schemas.

### 4.2 Tags API

Monorepo paths forbid Git-client tag pushes, so tags are managed through this entry point (ImportRepos can also push tags directly from Git clients, see §2.6):

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

### 4.6 ImportRepo removal

`POST /api/v1/import-repo/remove` detaches and sweeps one ImportRepo under `import_dir`; it is mounted only in the storage-only morphology. It requires `git.push_auth = "token"` and a token that covers the path; with `push_auth = "none"` it answers 403 to every valid request, so use the operator CLI `mega2 import-repo remove` instead. Removal semantics, retention and continuation are in §2.6; the request and response contract is in the "ImportRepo 叶子清理" section of [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) (Chinese).

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
| `mega2 import-repo remove --path <P> --yes [--cleanup-id <ID>] [--max-rounds <N>]` | Remove an ImportRepo on the server (storage-only; put `--config` before `import-repo` or set `MEGA_CONFIG`); outcome lines and exit codes in §2.6 |
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
| Repository paths, branches, tags, push behavior, and the ImportRepo lifecycle | Sections 1–2 of this guide |
| Trunk / storage-only deployment and the Compose stack | [`deployment.md`](./deployment.md) |
| Config keys and environment variables | [`config.toml`](../config/config.toml) (commented sample), [`configuration.md`](./configuration.md) |
| Git protocol surfaces and write path | [`architecture.md`](./architecture.md) |
| Directory, file, and tag operations | Sections 4.1–4.2 of this guide |
| OCI registry and Agent Capture routes | [`architecture.md`](./architecture.md) and [`deployment.md`](./deployment.md) |
| Error handling implementation | `crate::common::errors` |
| Contribution workflow and required checks | [`contributing.md`](./contributing.md) |
