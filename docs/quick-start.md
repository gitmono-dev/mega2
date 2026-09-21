# Quick Start

English · [中文](quick-start.zh.md)

This guide brings up the mega2 (trunk / storage-only) evaluation stack locally with the repository-root compose file for your platform — [`macos-orbstack-mega2-compose.yml`](../macos-orbstack-mega2-compose.yml) on macOS with OrbStack, [`linux-mega2-compose.yml`](../linux-mega2-compose.yml) on Linux Docker — and runs the first closed loop: HTTP clone → push to `main` → read back over the API. The stack pulls the **official release image from Docker Hub** (`genedna/mega2:latest`) — no source build, no bootstrap, no token. Product rules: [`monorepo.md`](./monorepo.md).

## Prerequisites

- macOS: OrbStack (the macOS compose file relies on its `*.orb.local` DNS). Linux: Docker Engine; the Linux compose file uses host networking for mega2 instead. Either way: the Compose plugin (v2), `git`, `curl`; run everything from the repository root.
- The first `up` pulls the `genedna/mega2:latest`, PostgreSQL, Redis, RustFS, and RustFS CLI images; duration depends on your network.

## Bring up the stack

```bash
# Pick the compose file for your platform:
COMPOSE=macos-orbstack-mega2-compose.yml   # macOS with OrbStack
COMPOSE=linux-mega2-compose.yml            # Linux Docker

docker compose -f $COMPOSE up -d --wait
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

You can only push to `main` (pushes of other branches are rejected); Git-client tag pushes are rejected too — create, list, and delete tags via the HTTP API or `libra mega2 browser` ([`monorepo.md`](./monorepo.md)).

### 3. Read back over the API + Swagger UI

```bash
# Read back the file you just pushed; this should print the contents of hello.md
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/hello.md"
```

Browse the full HTTP surface (Git smart HTTP, LFS, product writes, tags, optional OCI `/v2`) in Swagger UI: `http://127.0.0.1:9000/swagger-ui` (OpenAPI JSON: `/api/openapi.json`). For interactive terminal browsing use Libra's `libra mega2 browser`; mega2 itself serves no Web UI.

## Case: push nested directories (hierarchy is created automatically)

You **don't need to create directories in advance** in the monorepo. Just build a multi-level directory inside your `/project` clone and push — both `rust-lang/` and `rust-lang/crate/` are written by that push:

```bash
git clone http://127.0.0.1:9000/project
cd project
mkdir -p rust-lang/crate
echo "# crate" > rust-lang/crate/README.md
git add . && git commit -m "add rust-lang/crate"
git push origin main
```

After the push, the nested path is a subpath you can clone / push independently, and you can read it back over the API:

```bash
# Read back the file under the nested path
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/rust-lang/crate/README.md"

# Clone the subpath (never root-clone /)
git clone http://127.0.0.1:9000/project/rust-lang/crate
```

## Case: push an existing Git repository into mega2

To migrate an existing repository (keeping its branches and tags), use an **ImportRepo** under `/third-party`: repos there follow ordinary Git semantics — multi-branch and Git-client tags are both allowed ([`monorepo.md`](./monorepo.md)). If the target path does not exist yet, it is created automatically by the push; no prior setup is needed:

```bash
cd /path/to/your-repo
git remote add mega2 http://127.0.0.1:9000/third-party/your-repo
git push mega2 --all     # push all branches
git push mega2 --tags    # push all tags
```

Then clone / fetch as usual:

```bash
git clone http://127.0.0.1:9000/third-party/your-repo
```

Do not confuse the two path semantics:

- `/third-party/**` (ImportRepo): multi-branch and client tags allowed — suited for hosting third-party dependency sources and migrating existing repositories.
- Everywhere else (Monorepo): `main` is the only public branch and Git-client tags are forbidden. An existing repo's multi-branch history cannot be pushed straight into a monorepo subpath; the rules are in [`monorepo.md`](./monorepo.md).

## Case: mirror a GitHub repository (brewfs)

A full mirror of an existing GitHub repo — every branch and tag — takes one push. Using <https://github.com/brewfs/brewfs> as the example:

```bash
git clone --mirror https://github.com/brewfs/brewfs.git
cd brewfs.git
git lfs fetch --all origin                # best effort; one historic object is 404 on GitHub
git config lfs.allowincompletepush true   # push the LFS objects that are still available
git remote add mega2 http://127.0.0.1:9000/third-party/brewfs
git -c http.postBuffer=536870912 push --mirror mega2
```

Three things worth knowing:

- `--mirror` pushes **every** ref (all branches + all tags). The `git push mega2 --all` in the previous case only pushes *local* branches, which in a normal clone is usually just `main`.
- `-c http.postBuffer=536870912` is a temporary workaround: once the pack exceeds git's default 1 MiB `http.postBuffer`, the client switches to a chunked request body, which the current receive-pack endpoint rejects with HTTP 400. Raising the buffer keeps the request content-length'd.
- brewfs tracks large test fixtures with Git LFS. One object referenced by old history no longer exists on GitHub, so `git lfs fetch --all` prints a 404 error — expected; `lfs.allowincompletepush true` lets the push proceed without it.

Verify the round trip — annotated tags and LFS content both survive:

```bash
git clone http://127.0.0.1:9000/third-party/brewfs /tmp/verify-brewfs
cd /tmp/verify-brewfs
git for-each-ref refs/tags                       # v0.0.1 / v0.1.1 / v0.1.2
git cat-file -t v0.1.2^{tag}                     # => tag (annotated tag object intact)
ls -la tests/scripts/xfstests-prebuilt/xfstests-prebuilt.tar.gz
# ~8 MB gzip — the real bytes came back from mega2's LFS object store, not a pointer
```

## Case: large files with Git LFS

The stack speaks standard Git LFS (`/info/lfs`), so a stock `git-lfs` client works as-is. LFS write authorization shares `git.push_auth` with Git push — anonymous in this evaluation setup. Prerequisite: `git-lfs` on the host (check with `git lfs version`).

```bash
git clone http://127.0.0.1:9000/project
cd project
git lfs install --local                       # once per clone
mkdir -p lfs-demo && cd lfs-demo
git lfs track "*.bin"                         # writes .gitattributes
head -c 2097152 /dev/urandom > big.bin        # a 2 MiB binary
git add .gitattributes big.bin
git commit -m "add LFS-tracked big.bin"
git push origin main                          # "Uploading LFS objects: 100% (1/1)" runs first
```

The Git commit stores only an LFS pointer; the 2 MiB of bytes went to the object store during the push. Verify from a fresh clone of the subpath — checkout downloads the real content:

```bash
git clone http://127.0.0.1:9000/project/lfs-demo /tmp/verify-lfs
cmp lfs-demo/big.bin /tmp/verify-lfs/big.bin && echo identical
```

LFS objects live in the same object storage as Git blobs (RustFS in this stack), so everything said about volumes, `down -v`, and the persist override below covers them too.

## Observe, stop, and clean up

```bash
# Follow the mega2 logs
docker compose -f $COMPOSE logs -f mega2

# Stop; the named volumes keep the Postgres / Redis / RustFS / mega2 data
docker compose -f $COMPOSE down

# Also delete the data volumes (destructive; wipes this local evaluation instance)
docker compose -f $COMPOSE down -v
```

## Important: persist data to a local directory

The default stack uses Docker named volumes, so `down -v` deletes your data together with the volumes. If you plan to **use this instance long-term and want the data to outlive the stack itself**, switch the database, object storage, and mega2 data directories to bind mounts on the host — then **even if the containers and volumes are deleted, the data stays in the local directory and is picked up again on the next `up`**.

Create an override file `mega2-compose.persist.yml` in the repository root:

```yaml
# Layered on top of your chosen compose file: replace the named volumes with host bind mounts
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
docker compose -f $COMPOSE -f mega2-compose.persist.yml up -d --wait
```

From then on all data lives under `./mega2-data/`:

- `docker compose ... down -v` only removes named volumes — it **does not** touch `./mega2-data/`; the next `up` resumes from that directory with repositories, push history, and LFS objects intact.
- Backup = stop the stack and archive `./mega2-data/`.
- The directory is written by in-container processes (owned by container users such as postgres); do not commit it to version control and do not edit its contents by hand.

## The other Compose files

The repository ships two more Compose files, **both aimed at testing and development** — neither replaces this evaluation stack:

- [`docker/docker-compose-storage-only.yml`](../docker/docker-compose-storage-only.yml) (plus the `docker/docker-compose-storage-only.auth-none.yml` override): a trunk lab stack that **builds** the image from source, uses token authentication, and needs a `service init` bootstrap — used for deployment rehearsals and smoke tests. Usage: [`deployment.md`](./deployment.md) and [`deploy-trunk.md`](./deploy-trunk.md) §8.
- [`docker/docker-compose.test.yml`](../docker/docker-compose.test.yml): the integration-test (IT) data plane, see [`development.md`](./development.md).

## Next steps

- Day-to-day usage (clone / push / API / tags / LFS): [`user-guide.md`](./user-guide.md)
- Config keys, Profile, SecretRef, hot reload: [`configuration.md`](./configuration.md); commented sample at `config/config.toml`
- Deployment and operations (token management, morphology invariants, OCI, Agent Capture): [`deployment.md`](./deployment.md), [`deploy-trunk.md`](./deploy-trunk.md)
- Local development and tests: [`development.md`](./development.md)
