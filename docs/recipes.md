# Usage Recipes

English · [中文](recipes.zh.md)

Use these recipes after completing the [Quick Start](./quick-start.md). They cover nested directories, repository migration and mirroring, Git LFS, OCI images, build artifacts, and persistent local data.

## Push nested directories

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

## Migrate an existing Git repository

To migrate an existing repository (keeping its branches and tags), use an **ImportRepo** under `/third-party`: repos there follow ordinary Git semantics, so multiple branches and Git-client tags are allowed. If the target path does not exist yet, the push creates it automatically; no prior setup is needed. See the [User Guide](./user-guide.md) for path and branch behavior:

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
- Everywhere else (Monorepo): `main` is the only public branch and Git-client tags are forbidden. An existing repo's multi-branch history cannot be pushed straight into a Monorepo subpath; see the [User Guide](./user-guide.md).

## Mirror a GitHub repository (brewfs)

A full mirror of an existing GitHub repo — every branch and tag — takes one push. Using <https://github.com/brewfs/brewfs> as the example:

```bash
git clone --mirror https://github.com/brewfs/brewfs.git
cd brewfs.git
git lfs fetch --all origin                # best effort; one historic object is 404 on GitHub
git config lfs.allowincompletepush true   # push the LFS objects that are still available
git remote add mega2 http://127.0.0.1:9000/third-party/brewfs
git push --mirror mega2
```

Two things worth knowing:

- `--mirror` pushes **every** ref (all branches + all tags). The `git push mega2 --all` in the previous case only pushes *local* branches, which in a normal clone is usually just `main`.
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

## Store large files with Git LFS

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

## Store container images in the OCI registry

The stack serves an OCI Distribution registry under `/v2` (enabled in the Compose files with `MEGA_OCI__ENABLED=true`). It also requires storage-only mode, which `MEGA_GIT__PUSH_AUTH` already implies. Any OCI client works; Docker treats `127.0.0.1` as an insecure registry by default, so plain HTTP works for this local evaluation:

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

Registry writes share `git.push_auth` — anonymous in this evaluation setup. Blobs and manifests land in the same RustFS object storage as everything else. When you expose the registry to other machines, plain HTTP is no longer accepted by default Docker clients: put TLS in front of it, or add the host to the client's `insecure-registries`.

## Transfer build artifacts with presigned URLs

Compiled binaries, release tarballs and other build outputs are stored as **Artifact Sets** under `/api/v1/repos/{repo}/artifacts`. The flow is discovery → batch → commit, and the byte transfer is **presigned**: mega2 only handles metadata and signs time-limited (1 h) S3 URLs — the client uploads/downloads the bytes directly to/from the object storage (RustFS here), without proxying through mega2.

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

`missing_objects` must be empty — any object listed there was not found in the object storage and the commit recorded only the rest. Read the set back; downloads are presigned too, so `curl -L` follows the 302 straight to RustFS:

```bash
# List sets / resolve a file to its object id
curl -s "$BASE/sets?namespace=releases&object_type=snapshot"
curl -s "$BASE/resolve-file?namespace=releases&object_type=snapshot&path=app-1.0.0.tar.gz"

# Download: 302 redirect to a presigned GET, or JSON with the link (?mode=link)
curl -L -o dl.tar.gz "$BASE/objects/$OID1"
cmp app-1.0.0.tar.gz dl.tar.gz && echo identical
curl -s "$BASE/objects/$OID2?mode=link"
```

Artifact writes share the `git.push_auth` gate (anonymous here). The `object_type` vocabulary (`snapshot`, `provenance`, `run`, …) and the negotiated limits are advertised by the discovery payload.

## Check logs, stop, and clean up

```bash
# Follow the mega2 logs
docker compose -f $COMPOSE logs -f mega2

# Stop; the named volumes keep the Postgres / Redis / RustFS / mega2 data
docker compose -f $COMPOSE down

# Also delete the data volumes (destructive; wipes this local evaluation instance)
docker compose -f $COMPOSE down -v
```

## Persist data on the host

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

## Choose between the evaluation and test stacks

The repository ships two more Compose files, **both aimed at testing and development** — neither replaces this evaluation stack:

- [`docker/docker-compose-storage-only.yml`](../docker/docker-compose-storage-only.yml) (plus the `docker/docker-compose-storage-only.auth-none.yml` override): a trunk lab stack that **builds** the image from source, uses token authentication, and needs a `service init` bootstrap — used for deployment rehearsals and smoke tests. See the [Deployment Guide](./deployment.md).
- [`docker/docker-compose.test.yml`](../docker/docker-compose.test.yml): the integration-test (IT) data plane; see the [Contributing Guide](./contributing.md) for the local development workflow.
