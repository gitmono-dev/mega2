# Quick Start

English · [中文](quick-start.zh.md)

This guide brings up the mega2 (trunk / storage-only) evaluation stack locally with the repository-root [`mega2-compose.yml`](../mega2-compose.yml) and runs the first closed loop: HTTP clone → push to `main` → read back over the API. The stack pulls the **official release image from Docker Hub** (`genedna/mega2:latest`) — no source build, no bootstrap, no token. Product rules: [`monorepo.md`](./monorepo.md).

## Prerequisites

- Docker Engine with the Compose plugin (v2), `git`, `curl`; run everything from the repository root.
- The first `up` pulls the `genedna/mega2:latest`, PostgreSQL, Redis, RustFS, and RustFS CLI images; duration depends on your network.

## Bring up the stack

```bash
docker compose -f mega2-compose.yml up -d --wait
```

No initialization command is needed after `up`: `rustfs-init` creates the `mega2` bucket automatically, and mega2 initializes the empty Monorepo (`main` and the top-level directories) during service startup. The readiness probe is `/api/openapi.json`.

> **This is a local-only, anonymous setup**: the stack runs with `push_auth=none`, so clone / fetch / push need no credentials, and port 9000 is bound to `127.0.0.1` for that reason. Do not rebind it to `0.0.0.0` or expose it through a reverse proxy; shared or internet-facing deployments must use token authentication instead — see [`deployment.md`](./deployment.md) and [`deploy-trunk.md`](./deploy-trunk.md).

## First closed loop: clone → push → read back

### 1. Clone a subpath over HTTP

In storage-only mode you clone a subpath (do not root-clone `/`, see [`deploy-trunk.md`](./deploy-trunk.md) §9):

```bash
git clone http://127.0.0.1:9000/project
cd project
```

### 2. Push to main

`main` is the only public branch of the monorepo. With the anonymous setup the push needs no credentials:

```bash
echo "# hello mega2" > hello.md
git add hello.md && git commit -m "add hello.md"
git push origin main
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

## Observe, stop, and clean up

```bash
# Follow the mega2 logs
docker compose -f mega2-compose.yml logs -f mega2

# Stop; the named volumes keep the Postgres / Redis / RustFS / mega2 data
docker compose -f mega2-compose.yml down

# Also delete the data volumes (destructive; wipes this local evaluation instance)
docker compose -f mega2-compose.yml down -v
```

## Important: persist data to a local directory

The default stack uses Docker named volumes, so `down -v` deletes your data together with the volumes. If you plan to **use this instance long-term** (repository data must survive rebuilds), switch the database, object storage, and mega2 data directories to bind mounts on the host — then **even if the containers and volumes are deleted, the data stays in the local directory and is picked up again on the next `up`**.

Create an override file `mega2-compose.persist.yml` in the repository root:

```yaml
# Layered on top of mega2-compose.yml: replace the named volumes with host bind mounts
services:
  postgres:                              # metadata database
    volumes:
      - ./mega2-data/postgres:/var/lib/postgresql
  redis:                                 # cache / queues
    volumes:
      - ./mega2-data/redis:/data
  rustfs:                                # object storage (Git blobs / LFS)
    volumes:
      - ./mega2-data/rustfs:/data
  mega2:                                 # mega2 data directory (MEGA_BASE_DIR)
    volumes:
      - ./mega2-data/mega2:/var/lib/mega2
```

Start with both `-f` flags (use the same two `-f` flags for stop and cleanup commands):

```bash
docker compose -f mega2-compose.yml -f mega2-compose.persist.yml up -d --wait
```

From then on all data lives under `./mega2-data/`:

- `docker compose ... down -v` only removes named volumes — it **does not** touch `./mega2-data/`; the next `up` resumes from that directory with repositories, push history, and LFS objects intact.
- Backup = stop the stack and archive `./mega2-data/`.
- The directory is written by in-container processes (owned by container users such as postgres); do not commit it to version control and do not edit its contents by hand.

## The other Compose files

The repository ships two more Compose files, **both aimed at testing and development** — neither replaces this evaluation stack:

- [`docker-compose-storage-only.yml`](../docker-compose-storage-only.yml) (plus the `docker-compose-storage-only.auth-none.yml` override): a trunk lab stack that **builds** the image from source, uses token authentication, and needs a `service init` bootstrap — used for deployment rehearsals and smoke tests. Usage: [`deployment.md`](./deployment.md) and [`deploy-trunk.md`](./deploy-trunk.md) §8.
- [`docker-compose.test.yml`](../docker-compose.test.yml): the integration-test (IT) data plane, see [`development.md`](./development.md).

## Next steps

- Day-to-day usage (clone / push / API / tags / LFS): [`user-guide.md`](./user-guide.md)
- Config keys, Profile, SecretRef, hot reload: [`configuration.md`](./configuration.md); commented sample at `config/config.toml`
- Deployment and operations (token management, morphology invariants, OCI, Agent Capture): [`deployment.md`](./deployment.md), [`deploy-trunk.md`](./deploy-trunk.md)
- Local development and tests: [`development.md`](./development.md)
