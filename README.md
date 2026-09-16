# mega2

English · [中文](README.zh.md)

**mega2** is the successor engine to the same organization's [Mega](https://github.com/web3infra-foundation/mega) project: it continues Mega's shipped monorepo / Git hosting work as a deployable code-hosting and service backend.

Product rules: [`docs/monorepo.md`](docs/monorepo.md). Trunk / storage-only deploy: [`docs/deploy-trunk.md`](docs/deploy-trunk.md). Local development and tests: [`docs/development.md`](docs/development.md).

## Features

### Git hosting and monorepo

- **Monorepo**: only `refs/heads/main` is accepted as a public Git branch. Other heads (for example `refs/heads/dev`) are rejected at protocol validation. The Git client sees `ng <ref> …` (`trunk push rejects ref '…'; the only public branch is refs/heads/main`). This constrains Git receive-pack / `ls-remote` heads only; Agent Capture does not use this protocol ([see below](#agent-capture)).
  - **[Trunk-based development](https://trunkbaseddevelopment.com)**: a monorepo works best with a single trunk, not a tree of long-lived feature branches.
  - **No Change List in the open-source edition**: The Mega2 open-source edition ships the core monorepo storage capability and does not include Change List. A Change List implementation needs multiple branches, so this edition does not have multi-branch capability.
- **ImportRepo**: under `[monorepo].import_dir` (default `/third-party`), ordinary Git multi-branch and client tags apply. This is a Mega2 feature for developers to store the source of open-source third-party dependency libraries so they can use the latest versions of those libraries in local development.
- **Tags**: Monorepo forbids `git push --tags`. Create / list / delete go through the HTTP API only. Mega2 is a service-only open-source project; it is meant to be used together with Libra as the version-control tool. The `libra mega2` subcommands can manage directories, tags, and similar operations.
- **Object graph**: metadata in Postgres; blobs in pluggable object storage (local filesystem or S3-compatible object storage). `object_format` supports `sha1` (default) and the extensions `sha256` / `blake3` (these features require Libra as the version-control tool).

### Protocols and large files

- **Git Smart HTTP and SSH**: Mega2 speaks Smart HTTP and SSH to stock Git clients for clone / fetch / pull / push. Storage-only disables SSH receive-pack; SSH remains available for read-only fetch.
- **Git LFS**: large files are kept out of the ordinary Git object graph and use stock Git LFS (`/info/lfs` and `/api/v1/lfs`).
- **FastCDC Media**: on top of stock LFS, large media can be uploaded and reused as content-defined chunks (`--features fastcdc`). FastCDC and BLAKE3 support are Monorepo features built for large files and hash safety; they require Libra. Contract: [`docs/refactoring/fastcdc-media.md`](docs/refactoring/fastcdc-media.md).

### Two deployment forms

| Form | Use | Behavior |
|---|---|---|
| **review** (default) | Full platform | Cedar authorization, megaui login, Issue / reviewer product routes. Change List is not in the open-source edition |
| **trunk / storage-only** | Storage and distribution only | `push_policy=trunk`; pushes enter `main` through `MonoWriteQueue`; no CL / Issue / OAuth; `push_auth=token` or `none` |

Under trunk, the product write APIs (`POST /api/v1/create-entry`, `POST /api/v1/edit/save`) share tip authority with `git push`. Root-tree writes are globally serialized. Multi-commit pushes merge into `main` per product rules.

### HTTP API and extra surfaces

- REST plus runtime OpenAPI (`/api/openapi.json`) and Swagger UI.
- Read-only preview: blob / tree / blame.
- **OCI Distribution `/v2`** (storage-only only, and `[oci].enabled=true`): manifest / blob push and pull. See [`docs/refactoring/oci.md`](docs/refactoring/oci.md).
- <a id="agent-capture"></a>**Agent Capture** (storage-only only, and `[agent_capture].enabled=true`): standalone HTTP `/api/v1/agent-capture`, not a Git branch. Session, event, checkpoint, and file-op metadata live in `agent_capture_*` tables; bytes live in object-storage namespace `agent/` (`ObjectNamespace::Agent`). A checkpoint is an HTTP resource on the session (`…/checkpoints`). It does not write `refs/heads/*` and does not replay Libra's local `refs/libra/traces`. `agent_capture_blob_ref` is a table row that owns a blob, not a `refs/` name. Auth uses a separate ingest token; do not reuse Git `push_tokens`. See [`docs/refactoring/agent-capture.md`](docs/refactoring/agent-capture.md).
- **Post-commit outbound events** (`[storage_events]`, off by default): committed writes emit HMAC-signed HTTPS metadata events (`repo.push`, `oci.manifest.published`, `lfs.object.uploaded`, `lfs.media.finalized`, `agent_capture.events.committed`). Agent checkpoint outbound (`agent_capture.checkpoint.committed`) is not installed yet. See [`docs/refactoring/storage-events.md`](docs/refactoring/storage-events.md).

### Authn, authz, and secrets

- **Authentication** is in megaui (Better Auth session). **Authorization** is in mega2 (Cedar: `off` / `shadow` / `enforce`).
- Browsers use a session cookie → megaui `get-session`. Git / LFS / API use a Mono access token, an SSH public key, or a Bot token. Handbook: [`docs/manual/authz.md`](docs/manual/authz.md).
- Embedded **Vault** (crates.io `libvault` + `src/contract/vault/`): PKI / KV, SecretRef, fail-closed bootstrap.

### Notifications and config

- In-app notifications, plus optional Slack / webhook. Product mail is delivered through megaui's internal API; this repo does not run SMTP.
- First-class config module: `config init` / `validate` / secret, Profile, SecretRef, controlled hot reload. Default file `config/config.toml`, overridable with `--config` or `MEGA_CONFIG`.

## Quick tests with Compose

The integration stack is driven by this repo's `docker-compose.test.yml`. Object storage is inlined; a sibling `orbit` is **no longer** required. Full UI / login tests need sibling **megaui**:

```text
<parent>/
├── mega2/          # this repository (the directory may still use a historical checkout name)
└── megaui/         # frontend + auth (apps/web, apps/collab-server)
```

`website-next` / `website-db-init` / `megaui-collab` use `build.context` `../megaui`. Without megaui, the web profile build fails. Do **not** rename Compose service names, the isolated `website` database, or `MEGA_OAUTH__WEBSITE_*` / `MEGA_NOTIFICATION__WEBSITE_MAIL_*` — renaming fail-closes the session path into 401.

**Prerequisites**: Docker Compose v2; ports `17001` / `17002` / `19180` / `15432` / `16379` / `19000` free. The first run pulls `rust:1.97-bookworm` and `node:22-alpine` and compiles release; that takes a while.

### Full stack (recommended: web, then app)

Run from this repository root. `website-next` is a local tag with `pull_policy: never`. After pointing at megaui you must pass `--build`, or you may reuse an old image.

```bash
# 1) megaui: create the website DB + megaui-collab + website-next
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile web up -d --build --wait website-next

# 2) mega2: postgres / redis / rustfs + the engine
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app up -d --wait monoengine
```

You can also do one shot `--profile app --profile web up -d --build --wait`. mega2 does not `depends_on` `website-next` (cross-profile `depends_on` would reject an app-only compose). Two steps guarantee the frontend is healthy before the first `get-session`.

| Item | Value |
|---|---|
| megaui Web | `http://127.0.0.1:17001` |
| megaui Collab WebSocket | `ws://127.0.0.1:17002` |
| mega2 HTTP | `http://127.0.0.1:19180` |
| Engine → session base | `http://website-next:7001` |
| Accounts DB | isolated `website` database on the shared Postgres |
| CORS | already includes `http://127.0.0.1:17001` |

Page loop: open `http://127.0.0.1:17001`, register / log in → with cookie, `GET /api/v1/user` on `http://127.0.0.1:19180` → `username` / `website_user_id` match the megaui session; **no cookie → 401**.

Connectivity smoke:

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app --profile web exec -T monoengine \
  curl -fsS http://website-next:7001/api/auth/get-session
```

A browser session is not a Git credential. After login, still create an access token or register an SSH key under `/api/v1/user` for Git / LFS / SSH.

### Data plane + integration tests

When you do not need a standing engine and only want `cargo test`:

```bash
./scripts/dev-test.sh full    # data plane + git-cli + cargo test --all
./scripts/dev-test.sh down
```

`cp .env.test.example .env.test` first (the script also creates it). Hand-pasted steps and the port table: [`docs/development.md`](docs/development.md). The Compose project name **must** include `-p monoengine-it` so it does not steal the default directory-named network.

### Trunk / storage-only (no login surface)

Git HTTP + object storage, no megaui:

```bash
docker compose -p monoengine-trunk -f docker-compose-storage-only.yml up -d --wait

docker compose -p monoengine-trunk -f docker-compose-storage-only.yml exec -T monoengine \
  monoengine --config /etc/monoengine/config.toml service init --yes
```

HTTP: `http://127.0.0.1:9000/`. Default push token: `docs/deploy-trunk.md`. Ports do not collide with the IT stack; do not reuse the same Compose project name for both.

### Stop

```bash
docker compose -p monoengine-it -f docker-compose.test.yml --profile app --profile web down
# including volumes:
docker compose -p monoengine-it -f docker-compose.test.yml --profile app --profile web down -v

docker compose -p monoengine-trunk -f docker-compose-storage-only.yml down -v
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
