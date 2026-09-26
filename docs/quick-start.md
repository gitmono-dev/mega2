# Quick Start

English · [中文](quick-start.zh.md)

The original Mega was the first-generation monorepo platform; Mega2 is the second-generation engine built for Agents. Its core capabilities are Monorepo hosting and optional Agent Session Capture. The recommended Agent setup combines Mega2 with ScorpioFS, which mounts repository paths as a local filesystem, and Libra, which provides Agent version-control workflows. This quick start focuses on Git operations against the Monorepo; Agent Session Capture must be enabled separately (see the [Deployment Guide](./deployment.md) and [Configuration Reference](./configuration.md)).

Start the local mega2 evaluation stack with the Compose file for your platform: [`macos-orbstack-mega2-compose.yml`](../macos-orbstack-mega2-compose.yml) on macOS with OrbStack, or [`linux-mega2-compose.yml`](../linux-mega2-compose.yml) on Linux. You will clone a repository over HTTP, push to `main`, and read the result back through the API. The remaining examples cover repository migration, Git LFS, OCI images, build artifacts, and data persistence. The stack pulls the **official release image from Docker Hub** (`genedna/mega2:latest`); it does not build from source and needs no bootstrap command or token. For branch and tag behavior, see the [User Guide](./user-guide.md).

## Prerequisites

- **macOS:** OrbStack; the macOS Compose file relies on its `*.orb.local` DNS.
- **Linux:** Docker Engine; the Linux Compose file uses host networking for mega2.
- **Both:** Docker Compose v2, `git`, and `curl`. Run the commands below from the repository root.
- The first `up` pulls `genedna/mega2:latest`, PostgreSQL, Redis, RustFS, and the RustFS CLI images. Pull time depends on your network.

## Bring up the stack

```bash
# Pick the compose file for your platform:
COMPOSE=macos-orbstack-mega2-compose.yml   # macOS with OrbStack
COMPOSE=linux-mega2-compose.yml            # Linux Docker

docker compose -f $COMPOSE up -d --wait
```

No separate initialization step is needed. `rustfs-init` creates the `mega2` bucket, and mega2 initializes the empty Monorepo—with `main` and the top-level directories—when the service starts. The readiness endpoint is `/api/openapi.json`.

> **For local use only:** this stack sets `push_auth=none`, so Git clone, fetch, and push require no credentials. Port 9000 is therefore bound to `127.0.0.1`. Do not bind it to `0.0.0.0` or expose it through a reverse proxy. For shared or internet-facing deployments, configure token authentication; see the [Deployment Guide](./deployment.md).

## Clone, push, and verify

### 1. Clone a subpath over HTTP

In storage-only mode, clone a repository subpath; cloning the root path `/` is unsupported.

```bash
git clone http://127.0.0.1:9000/project
cd project
```

### 2. Push to main

`main` is the monorepo's only public branch. In this local anonymous setup, the push needs no credentials:

```bash
echo "# hello mega2" > hello.md
git add hello.md && git commit -m "add hello.md"
git push origin main
```

On Monorepo paths, pushes to branches other than `main` and tag pushes from Git clients are rejected (ImportRepos under `/third-party` are exempt, see "Migrate an existing Git repository" below). Create, list, and delete tags through the HTTP API or the Libra command `libra mega2 browser`; see the [User Guide](./user-guide.md).

### 3. Read back over the API + Swagger UI

```bash
# Read back the file you just pushed; this should print the contents of hello.md
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/hello.md"
```

Explore the HTTP API—including Git Smart HTTP, LFS, product writes, tags, and optional OCI `/v2`—in Swagger UI at `http://127.0.0.1:9000/swagger-ui`. The OpenAPI document is at `/api/openapi.json`. Mega2 has no Web UI; use `libra mega2 browser` for basic terminal directory browsing and tag operations. Git clone, fetch, and push still use a Git client.

## Example: create a repository at a new path

A repository created with `git init` cannot be pushed straight to a Monorepo path (outside `/third-party`) that does not exist yet: the server rejects it with `MONO_PATH_UNINITIALIZED` and asks you to provision the path first. Provisioning only creates the directory and is safe to repeat; after it, clone the path, commit on top and push. Allowed roots, the three first-push cases and the error codes are in the User Guide's ["Monorepo path policy and first use"](./user-guide.md#25-monorepo-path-policy-and-first-use).

```bash
# Provision /project/demo inside the mega2 container (this evaluation stack needs no token); the in-container port depends on the compose file
case "$COMPOSE" in macos-*) PORT=8000 ;; *) PORT=9000 ;; esac
docker compose -f $COMPOSE exec mega2 mega2 path provision --server http://127.0.0.1:$PORT /project/demo

# Clone the provisioned path, commit and push
git clone http://127.0.0.1:9000/project/demo
cd demo
echo "# demo" > README.md
git add README.md && git commit -m "add README.md"
git push origin main
```

`mega2 path provision` needs an image of 0.39.4 or later; for a stack pulled earlier, re-run `docker compose -f $COMPOSE up -d --wait` first to pull the current image. A successful run prints `created /project/demo (<commit>)`; running it again prints `already exists /project/demo`. With the mega2 CLI installed locally you can also run `mega2 path provision --server http://127.0.0.1:9000 /project/demo` directly; deployments that require a token take it from the environment variable `MEGA2_TOKEN`.

## Example: push nested directories

You **don't need to create directories in advance**. Create a nested directory in your `/project` clone and push; mega2 creates both `rust-lang/` and `rust-lang/crate/` as part of that push:

```bash
git clone http://127.0.0.1:9000/project
cd project
mkdir -p rust-lang/crate
echo "# crate" > rust-lang/crate/README.md
git add . && git commit -m "add rust-lang/crate"
git push origin main
```

After the push, you can clone or push to the nested path independently, or read its files through the API:

```bash
# Read back the file under the nested path
curl -fsS "http://127.0.0.1:9000/api/v1/blob?path=/project/rust-lang/crate/README.md"

# Clone the subpath (never root-clone /)
git clone http://127.0.0.1:9000/project/rust-lang/crate
```

## Example: migrate an existing Git repository

To preserve an existing repository's branches and tags, migrate it under `/third-party` as an **ImportRepo**. ImportRepo paths follow ordinary Git semantics, so multiple branches and Git-client tags are allowed. The push creates the target path if it does not exist:

```bash
cd /path/to/your-repo
git remote add mega2 http://127.0.0.1:9000/third-party/your-repo
git push mega2 --all     # push all branches
git push mega2 --tags    # push all tags
```

You can then clone or fetch it as usual:

```bash
git clone http://127.0.0.1:9000/third-party/your-repo
```

An ImportRepo can keep being updated; push new commits and tags again:

```bash
cd /path/to/your-repo
echo "next" >> CHANGELOG.md
git add CHANGELOG.md && git commit -m "next change"
git tag -a v1.1 -m "v1.1"
git push mega2 --all
git push mega2 --tags
```

`--all` and `--tags` push only the branches and tags you have locally. If a push is rejected (usually the Git client rejects it first with `fetch first`; a ref that another push changed after the server's advertisement is rejected with `IMPORT_REPO_STALE_REF`), run `git fetch mega2`, merge or rebase, and push again; a conflicting tag has to be resolved on its own.

To make the ImportRepo follow all upstream branches and tags, use a `--mirror` clone as in the brewfs example below and run `git fetch --prune origin && git push --mirror mega2` for each sync. `--mirror` makes the ImportRepo an exact copy of the mirror clone: branches and tags that upstream does not have (such as `v1.1`, pushed straight to mega2 above) are deleted or force-overwritten. If the repository uses Git LFS, run `git fetch --prune origin`, `git lfs fetch --all origin`, `git lfs push --all mega2` and `git push --mirror mega2` in that order for each sync, so the LFS objects reach mega2 too. When upstream is missing some LFS objects (as brewfs is), `git lfs fetch` exits non-zero, which is expected; do not stop there: set `git config lfs.allowincompletepush true` as in the brewfs example and run the last two steps.

When you no longer need it, remove it. This evaluation stack runs with `push_auth=none`, where the HTTP removal endpoint answers 403 to every valid request, so go back to the directory of the compose file and run the operator CLI inside the mega2 container (`-e MEGA_LOG__PRINT_STD=false` leaves only the outcome line on stdout):

```bash
docker compose -f $COMPOSE exec -T -e MEGA_LOG__PRINT_STD=false mega2 mega2 import-repo remove --path /third-party/your-repo --yes
```

The second push needs an image of 0.40.2 or later, and `mega2 import-repo remove` needs 0.40.11 or later; for a stack pulled earlier, re-run `docker compose -f $COMPOSE up -d --wait` first to pull the current image. A successful run prints `removed /third-party/your-repo (repo_id=<n>, cleanup_id=<n>)` and exits 0; afterwards a clone of the path returns 404, and pushing again imports it as a new repository. If it prints `pending …` (exit code 3), continue with `--cleanup-id` as stderr suggests. Under `exec -T`, Ctrl-C only ends the Compose client while the removal in the container keeps running, so do not rerun right away. Other outcomes and exit codes, retention, and HTTP removal with a token are in the User Guide's ["ImportRepo lifecycle"](./user-guide.md#26-importrepo-lifecycle).

The two path types have different rules:

- `/third-party/**` (ImportRepo): multiple branches and client tags are allowed, later pushes update it, and it can be removed. Use this path for third-party dependencies or repositories you are migrating.
- All other paths (Monorepo): `main` is the only public branch, and Git-client tag operations are rejected. You cannot push a multi-branch repository directly into a Monorepo subpath.

## Example: mirror a GitHub repository (brewfs)

This example mirrors every branch and tag from <https://github.com/brewfs/brewfs> in a single push:

```bash
git clone --mirror https://github.com/brewfs/brewfs.git
cd brewfs.git
git lfs fetch --all origin                # best effort; one historic object is 404 on GitHub
git config lfs.allowincompletepush true   # push the LFS objects that are still available
git remote add mega2 http://127.0.0.1:9000/third-party/brewfs
git push --mirror mega2
```

Keep these details in mind:

- `--mirror` pushes **every** ref, including all branches and tags. By contrast, `git push mega2 --all` pushes only local branches, which in a standard clone usually means `main`.
- brewfs stores large test fixtures in Git LFS. One object referenced in its history is no longer available on GitHub, so `git lfs fetch --all` reports a 404. That is expected; setting `lfs.allowincompletepush` to `true` lets you push the objects that are still available.

Verify that annotated tags and LFS content made the round trip:

```bash
git clone http://127.0.0.1:9000/third-party/brewfs /tmp/verify-brewfs
cd /tmp/verify-brewfs
git for-each-ref refs/tags                       # v0.0.1 / v0.1.1 / v0.1.2
git cat-file -t v0.1.2^{tag}                     # => tag (annotated tag object intact)
ls -la tests/scripts/xfstests-prebuilt/xfstests-prebuilt.tar.gz
# ~8 MB gzip — the real bytes came back from mega2's LFS object store, not a pointer
```

## Example: store large files with Git LFS

The stack supports standard Git LFS at `/info/lfs`, so the standard `git-lfs` client works without extra configuration. LFS writes use the same `git.push_auth` setting as Git pushes; this evaluation stack allows anonymous writes. Install `git-lfs` on the host and confirm it is available with `git lfs version`.

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

The Git commit contains an LFS pointer; the 2 MiB file itself is uploaded to object storage during the push. Clone the subpath into a fresh directory to verify that checkout downloads the actual file:

```bash
git clone http://127.0.0.1:9000/project/lfs-demo /tmp/verify-lfs
cmp lfs-demo/big.bin /tmp/verify-lfs/big.bin && echo identical
```

LFS objects share object storage with Git blobs (RustFS in this stack). The volume and persistence guidance below applies to both.

## Example: store container images in the OCI registry

The stack exposes an OCI Distribution registry at `/v2`; the Compose files enable it with `MEGA_OCI__ENABLED=true`. The registry requires storage-only mode, which is already selected by `MEGA_GIT__PUSH_AUTH`. Any OCI client can use it. Docker treats `127.0.0.1` as an insecure registry by default, so HTTP works for this local evaluation:

```bash
docker pull alpine:3.21
docker tag alpine:3.21 127.0.0.1:9000/project/alpine:quickstart
docker push 127.0.0.1:9000/project/alpine:quickstart

# Read back: wipe the local tags and pull from mega2
docker rmi 127.0.0.1:9000/project/alpine:quickstart alpine:3.21
docker pull 127.0.0.1:9000/project/alpine:quickstart
docker run --rm 127.0.0.1:9000/project/alpine:quickstart cat /etc/alpine-release

# Or query the registry API directly
curl -s http://127.0.0.1:9000/v2/project/alpine/tags/list
```

Registry writes use the same `git.push_auth` setting as Git pushes; this evaluation stack allows anonymous writes. Blobs and manifests share the RustFS object store. For access from other machines, configure TLS or add the registry host to each Docker client's `insecure-registries` list.

## Example: transfer build artifacts with presigned URLs

Store binaries, release archives, and other build outputs as **Artifact Sets** under `/api/v1/repos/{repo}/artifacts`. The workflow uses three requests: discovery, batch, and commit. mega2 handles metadata and issues S3 URLs that expire after one hour. Clients upload and download file bytes directly to object storage (RustFS in this example).

```bash
REPO=project   # single URL path segment; use %2F for "/" inside names
BASE=http://127.0.0.1:9000/api/v1/repos/$REPO/artifacts

# 0. Discovery: protocol version, limits, supported transfers
curl -s $BASE/discovery

# 1. Stage two release files; each object id is a client-generated UUID
head -c 1048576 /dev/urandom > app-1.0.0.tar.gz
shasum -a 256 app-1.0.0.tar.gz > SHA256SUMS
OID1=$(uuidgen | tr 'A-Z' 'a-z'); OID2=$(uuidgen | tr 'A-Z' 'a-z')

# 2. Batch: declare the objects, get a presigned PUT URL per object
curl -s -X POST -H 'Content-Type: application/json' $BASE/batch --data @- <<EOF
{"namespace": "releases", "object_type": "snapshot", "intent": "upload",
 "objects": [
   {"path": "app-1.0.0.tar.gz", "oid": "$OID1", "size": $(wc -c < app-1.0.0.tar.gz | tr -d ' '), "content_type": "application/gzip"},
   {"path": "SHA256SUMS", "oid": "$OID2", "size": $(wc -c < SHA256SUMS | tr -d ' '), "content_type": "text/plain"}],
 "metadata": {"run_id": "quickstart-demo"}}
EOF
# → {"transfer":"basic","objects":[{"oid":"...","exists":false,
#    "actions":{"upload":{"href":"http://<rustfs>/...?X-Amz-Signature=...",
#    "header":{"Content-Type":"..."},"expires_at":"..."}}}], ...}

# 3. Upload the bytes STRAIGHT to the object storage (not to mega2).
#    Send the headers returned in actions.upload.header:
curl -X PUT -H 'Content-Type: application/gzip' --data-binary @app-1.0.0.tar.gz "<href for OID1>"
curl -X PUT -H 'Content-Type: text/plain' --data-binary @SHA256SUMS "<href for OID2>"

# 4. Commit: register the uploaded objects as one Artifact Set
curl -s -X POST -H 'Content-Type: application/json' $BASE/commit --data @- <<EOF
{"namespace": "releases", "object_type": "snapshot",
 "files": [
   {"path": "app-1.0.0.tar.gz", "oid": "$OID1", "size": $(wc -c < app-1.0.0.tar.gz | tr -d ' ')},
   {"path": "SHA256SUMS", "oid": "$OID2", "size": $(wc -c < SHA256SUMS | tr -d ' ')}],
 "metadata": {"run_id": "quickstart-demo"}}
EOF
# → {"artifact_set_id":"...","status":"ok","missing_objects":[]}
```

Confirm that `missing_objects` is empty. Any object listed there was missing from storage and was left out of the committed set. To download a file, use its presigned URL; `curl -L` follows the redirect to RustFS:

```bash
# List sets / resolve a file to its object id
curl -s "$BASE/sets?namespace=releases&object_type=snapshot"
curl -s "$BASE/resolve-file?namespace=releases&object_type=snapshot&path=app-1.0.0.tar.gz"

# Download: 302 redirect to a presigned GET, or JSON with the link (?mode=link)
curl -L -o dl.tar.gz "$BASE/objects/$OID1"
cmp app-1.0.0.tar.gz dl.tar.gz && echo identical
curl -s "$BASE/objects/$OID2?mode=link"
```

Artifact writes use the same `git.push_auth` setting as Git pushes; this stack allows anonymous writes. The discovery response lists supported `object_type` values (such as `snapshot`, `provenance`, and `run`) and negotiated limits.

## Check logs, stop, and clean up

```bash
# Follow the mega2 logs
docker compose -f $COMPOSE logs -f mega2

# Stop; the named volumes keep the Postgres / Redis / RustFS / mega2 data
docker compose -f $COMPOSE down

# Also delete the data volumes (destructive; wipes this local evaluation instance)
docker compose -f $COMPOSE down -v
```

## Example: persist data on the host

The default stack stores data in Docker named volumes, which `down -v` removes. To keep data after removing the stack, bind-mount the database, object store, and mega2 data directory to host paths. Those files remain after the containers and volumes are removed, and the next `up` reuses them.

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

Start the stack with both `-f` flags. Use the same flags when stopping or cleaning it up:

```bash
docker compose -f $COMPOSE -f mega2-compose.persist.yml up -d --wait
```

All persistent data now lives under `./mega2-data/`:

- `docker compose ... down -v` removes named volumes but **does not** touch `./mega2-data/`. The next `up` reuses that directory, including its repositories, push history, and LFS objects.
- To back up the instance, stop the stack and archive `./mega2-data/`.
- Container processes own and write these files. Do not commit the directory or edit its contents by hand.

## Choose between the evaluation and test stacks

The repository ships two more Compose files, **both aimed at testing and development** — neither replaces this evaluation stack:

- [`docker/docker-compose-storage-only.yml`](../docker/docker-compose-storage-only.yml) (plus the `docker/docker-compose-storage-only.auth-none.yml` override): a trunk lab stack that **builds** the image from source, uses token authentication, and needs a `service init` bootstrap — used for deployment rehearsals and smoke tests. See the [Deployment Guide](./deployment.md).
- [`docker/docker-compose.test.yml`](../docker/docker-compose.test.yml): the integration-test (IT) data plane, see the [Contributing Guide](./contributing.md) for the local development workflow.

## Next steps

- Day-to-day usage (clone / push / API / tags / LFS): [`user-guide.md`](./user-guide.md)
- Config keys, Profile, SecretRef, hot reload: [`configuration.md`](./configuration.md); commented sample at `config/config.toml`
- Deployment and operations (token management, mode invariants, OCI, Agent Capture): [`deployment.md`](./deployment.md)
- Contribution workflow and required checks: [`contributing.md`](./contributing.md)
