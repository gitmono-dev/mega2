# Quick Start

English · [中文](quick-start.zh.md)

This guide brings up mega2 (trunk / storage-only) locally with the in-repo Compose stack and runs the first closed loop: HTTP clone → token push to `main` → read back over the API. Product rules: [`monorepo.md`](./monorepo.md). Deploy/ops source of truth: [`deploy-trunk.md`](./deploy-trunk.md). This page keeps only the minimal runnable path and does not duplicate the config keys, port tables, or token values owned by those docs.

## Prerequisites

- Docker Compose v2, `git`, `curl`; run everything from the repository root.
- Time: about 10 minutes once the image exists. The first `up` builds the `mega2:local` image (a Rust release build — noticeably longer; build definition in the root `Dockerfile`).

## Bring up the Compose stack

Stack contents (mega2 HTTP 9000 / SSH 2222, plus Postgres / Redis / RustFS) and ports are documented in the header comments of `docker-compose-storage-only.yml`; they are not copied here.

The push token is injected as a Docker secret file (`${file:/run/secrets/mega2-push-token}` on the config side; see `config/config-storage-only.toml`). The file is not committed, so create it first:

```bash
mkdir -p secrets
openssl rand -hex 16 > secrets/mega2-push-token.local
```

> If you later run the repo's smoke scripts, the file must instead contain the local default token registered in [`deploy-trunk.md`](./deploy-trunk.md) §8 (that doc is the single source of truth for the value; it is not copied here). A self-generated random token works equally well for this guide's loop.

Start the stack and wait for the healthchecks (mega2's readiness probe is `/api/openapi.json`):

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml up -d --wait
```

The object store defaults to RustFS (s3compatible); do **not** add `--env-file` for the default bring-up — it is only needed when switching mega2 to the local-filesystem backend, see [`deploy-trunk.md`](./deploy-trunk.md) §8.

## Bootstrap (empty-volume init)

After the first start, initialize the repository: create `main`, the admin, and the top-level directories agreed in [`monorepo.md`](./monorepo.md) (`/project`, `/third-party`, …):

```bash
docker compose -p mega2-trunk -f docker-compose-storage-only.yml exec -T mega2 \
  mega2 --config /etc/mega2/config.toml service init --yes
```

Run once per empty volume; re-run after a `down -v` rebuild.

## First closed loop: clone → push → read back

### 1. Clone a subpath over HTTP

In storage-only mode you clone a subpath (do not root-clone `/`, see [`deploy-trunk.md`](./deploy-trunk.md) §9). The sample config sets `git.anonymous_access=true`, so reads need no credentials:

```bash
git clone http://127.0.0.1:9000/project
cd project
```

### 2. Push to main with the push token

`main` is the only public branch of the monorepo. Pushes authenticate over HTTP Basic: the username is arbitrary and only the password (the token bytes) takes part in the decision ([`deploy-trunk.md`](./deploy-trunk.md) §4):

```bash
TOKEN=$(cat ../secrets/mega2-push-token.local)
echo "# hello mega2" > hello.md
git add hello.md && git commit -m "add hello.md"
git push "http://x:${TOKEN}@127.0.0.1:9000/project" main
```

Pushes enter `main` through the MonoWriteQueue, sharing tip authority with the product API writes ([`monorepo.md`](./monorepo.md)). Git-client tag pushes are rejected; manage tags via the HTTP API or `libra mega2 browser`.

### 3. Read back over the API + Swagger UI

```bash
# Tree object download (binary stream); 200 means the read-back succeeded
curl -fsS -o /dev/null -w '%{http_code}\n' "http://127.0.0.1:9000/api/v1/file/tree?path=/project"

# The file you just pushed
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/hello.md"
```

Browse the full HTTP surface (Git smart HTTP, LFS, product writes, tags, optional OCI `/v2`) in Swagger UI: `http://127.0.0.1:9000/swagger-ui` (OpenAPI JSON: `/api/openapi.json`). For interactive terminal browsing use Libra's `libra mega2 browser`; mega2 itself serves no Web UI.

## Stop and clean up

```bash
# Stop, keeping the data volumes
docker compose -p mega2-trunk -f docker-compose-storage-only.yml down

# Also delete the data volumes (destructive; re-run the bootstrap afterwards)
docker compose -p mega2-trunk -f docker-compose-storage-only.yml down -v
```

## Next steps

- Day-to-day usage (clone / push / API / tags / LFS): [`user-guide.md`](./user-guide.md)
- Config keys, Profile, SecretRef, hot reload: [`configuration.md`](./configuration.md); commented sample at `config/config.toml`
- Deployment and operations (token management, morphology invariants, OCI, Agent Capture): [`deployment.md`](./deployment.md), [`deploy-trunk.md`](./deploy-trunk.md)
- Stack-level protocol / API-write smoke (git, LFS black-box): [`deploy-trunk.md`](./deploy-trunk.md) §8.1
- Local development and tests: [`development.md`](./development.md)
