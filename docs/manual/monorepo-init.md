English · [中文](monorepo-init.zh.md)

# Monorepo Initialization and Directory Layout

This manual explains how mega2 creates the initial Monorepo tree, how to
configure its top-level directories, and what the first commit contains. For
branch, tag, and ImportRepo behavior, see the [User Guide](../user-guide.md).

> **Scope:** first-time initialization of the Monorepo root and paths outside
> `import_dir`.
> **Not covered:** ImportRepos under `import_dir`, or rebuilding the tree of an
> already initialized database. mega2 does not rebuild that tree from config.

## 1. When initialization runs

On service startup, `Context::new` calls `MonoService::init_monorepo`. If a
`main` ref already exists at `/`, initialization is skipped, so the operation
is idempotent and only creates the layout for a new database. The initial
commit, `main` ref, and tree/blob metadata are written in one Postgres
transaction; raw blob contents go to the configured object store.

For a one-time bootstrap without starting listeners, run:

```bash
mega2 --config config/config.toml service init --yes
```

The command uses the existing configuration and exits after creating the
initial graph. It does not start the HTTP or Git listeners.

## 2. Configuration inputs

Initialization is driven by `[monorepo]` in the config file. The commented
[`config.toml`](../../config/config.toml) lists every supported setting.

| Setting | Initialization behavior |
|---|---|
| `root_dirs` | Creates one top-level directory per entry, each with its own `.gitkeep` placeholder. |
| `import_dir` | Defaults to `/third-party`. It does not create a directory; it marks paths below it as ImportRepos. Keep it aligned with an entry in `root_dirs`. |
| `admin` | Seeds the root `.mega_cedar.json` entity and the default repository reviewer policy. |
| `rename.*` | Controls diff rename detection; it does not affect the initial tree. |

`import_dir`, `root_dirs`, and `admin` must be non-empty. They are restart-
required settings, and changing them affects only a database that has not yet
been initialized. Existing trees are not rebuilt. `root_dirs` supports only
single path components; do not include `/` in an entry (see section 5).

## 3. What the initial commit contains

`MegaModelConverter::init` and `init_trees` in
`src/jupiter/utils/converter.rs` assemble and sort the root tree. The initial
commit contains:

1. Each configured `root_dirs` entry with a `.gitkeep` blob. Placeholder
   contents include the directory name, so each directory has a distinct tree
   hash.
2. A root-level `.mega_cedar.json` entity generated from `admin`.
3. `.cedar/policies.cedar`, which makes `admin` the default reviewer for the
   repository (or is empty when no admin policy is generated).
4. Buck placeholders `.buckroot` and `.buckconfig`.
5. `toolchains/BUCK` when `toolchains` appears in `root_dirs`.
6. An initial, parentless commit named `Init Mega Directory`, referenced by
   `refs/heads/main`.

Initialization creates no Git tags and no additional public branches.

## 4. Suggested top-level layout

The repository's sample config uses this eight-directory layout:

```toml
root_dirs = ["third-party", "project", "doc", "artifact", "release", "model", "data", "toolchains"]
```

The engine gives special treatment only to the `import_dir` boundary and the
`toolchains/BUCK` injection. Other names are team conventions; each starts as
an empty directory containing `.gitkeep`.

### Suggested use by directory

| Directory | Suggested use |
|---|---|
| `third-party/` | Root for imported repositories. Keep it aligned with `import_dir`. |
| `project/` | Application and service source code. |
| `doc/` | Design notes, specifications, and review material. |
| `artifact/` | Optional repository convention for release files; separate from mega2's artifact storage API. |
| `release/` | Versioned release records and notes. |
| `model/` | Model assets; use Git LFS for large files. |
| `data/` | Shared datasets and samples; use Git LFS for large files. |
| `toolchains/` | Buck2 toolchain definitions; mega2 also adds a demo `BUCK` file. |

### Resulting root tree

With the sample configuration, the logical root looks like this:

```text
/
├── .buckconfig
├── .buckroot
├── .cedar/policies.cedar
├── .mega_cedar.json
├── third-party/.gitkeep
├── project/.gitkeep
├── doc/.gitkeep
├── artifact/.gitkeep
├── release/.gitkeep
├── model/.gitkeep
├── data/.gitkeep
└── toolchains/
    ├── .gitkeep
    └── BUCK
```

### How `import_dir` classifies paths

The actual tree always follows the running config's `root_dirs`. The sample
and `MonoConfig::default` do not necessarily include all eight suggested
directories. Set the desired list before the first initialization.

`import_dir` defaults to `/third-party` and classifies paths by prefix,
regardless of whether the directory exists in the tree. The directory itself
comes from `root_dirs`; `import_dir` does not create it. If the two settings
drift apart, a path may be visible but use Monorepo rules, or use ImportRepo
rules without a matching directory in the initial layout.

## 5. Configure the layout safely

Before the first service startup:

1. Set `[monorepo].root_dirs` in `config/config.toml`.
2. Keep `import_dir` aligned with the intended ImportRepo root.
3. Set `admin` to the system administrator accounts used by the deployment.
4. Validate the config:

   ```bash
   cargo run -p mega2 -- --config config/config.toml config validate
   ```

5. Start the service against a new or uninitialized database. Verify the root
   with `git ls-remote` or clone it and inspect it with `git ls-tree`.

### Directory depth: top-level entries only

Each `root_dirs` string is written as one literal tree-entry name; mega2 does
not split entries on `/` to create nested directories. For example,
`root_dirs = ["a/b"]` does not create `a/` containing `b/`; it produces an
invalid Git tree entry that clients cannot traverse. Use simple names such as
`a`, then create `a/b` with a normal Git commit after cloning.

The current config validator checks that entries are non-empty but does not
reject slashes. Until that validation is tightened, avoid `/` and `\\` in
`root_dirs`. To change a layout after initialization, commit directory changes
normally. Reinitializing requires resetting the data plane and is destructive;
this manual does not provide reset steps.

## 6. Container configuration

### Config file lookup and overlays

The config loader chooses the first available file in this order:

1. `--config <path>`
2. `MEGA_CONFIG`
3. `./config/config.toml`
4. `{MEGA_BASE_DIR}/etc/config.toml`

If none exists, mega2 creates a default config at the `MEGA_BASE_DIR` path.
It then overlays the base file, an optional profile file (`config.<name>.toml`),
and environment variables in that order. Environment overrides use
`MEGA_<SECTION>__<KEY>`, for example `MEGA_DATABASE__DB_URL`. Unknown config
keys are rejected; `MEGA_MAIL__*` is no longer supported.

### Image defaults

The release image includes the sample config and points the binary at it:

```dockerfile
COPY mega2/config/config.toml /etc/mega2/config.toml
ENV MEGA_BASE_DIR=/var/lib/mega2 MEGA_CONFIG=/etc/mega2/config.toml
ENTRYPOINT ["/usr/local/bin/mega2"]
CMD ["--config", "/etc/mega2/config.toml", "service", "http", "--host", "0.0.0.0", "-p", "8000"]
```

### Production mounts

For production, mount a read-only config and persist the data directory. Keep
environment-specific endpoints and credentials in environment variables or
secrets rather than baking plaintext into an image:

```bash
docker run -d --name mega2 \
  -v /srv/mega2/config.toml:/etc/mega2/config.toml:ro \
  -v mega2-data:/var/lib/mega2 \
  -e MEGA_DATABASE__DB_URL='postgres://user:***@postgres:5432/mega2' \
  -e MEGA_REDIS__URL='redis://redis:6379' \
  -p 8000:8000 mega2:local
```

### Validate and apply mounted config

Database credentials cannot use Vault SecretRef because database access is
needed before Vault can be initialized. Other supported credentials can use
`vault://` SecretRefs and are resolved at runtime. Validate a mounted config
before starting the service:

```bash
docker run --rm -v /srv/mega2/config.toml:/etc/mega2/config.toml:ro \
  mega2:local config validate --deny-warnings
```

Finalize `root_dirs`, `import_dir`, and `admin` before the first startup. A
restart applies config changes, but does not regenerate an existing tree.

## 7. Storage model

The logical tree is not a Git bare repository on disk. Postgres stores commit,
tree, blob metadata, and refs; the configured object store (local filesystem,
S3-compatible storage, or GCS) stores blob contents. See the
[Architecture Guide](../architecture.md) for storage boundaries.

## 8. Related references

| Topic | Reference |
|---|---|
| Paths, branches, tags, and ImportRepo behavior | [User Guide](../user-guide.md) |
| Configuration keys and defaults | [`config.toml`](../../config/config.toml) |
| Protocol behavior and service surfaces | [Architecture Guide](../architecture.md) |
| Initialization implementation | `src/jupiter/service/mono_service.rs::init_monorepo`, `src/jupiter/utils/converter.rs` |
| Local development and tests | [`../contributing.md`](../contributing.md) |
