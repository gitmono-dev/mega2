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
| `import_dir` | Defaults to `/third-party`. It does not create a directory; it marks paths below it as ImportRepos. Its first path component must be listed in `root_dirs`. |
| `admin` | Seeds the root `.mega_cedar.json` entity and the default repository reviewer policy. |
| `rename.*` | Controls diff rename detection; it does not affect the initial tree. |

`import_dir`, `root_dirs`, and `admin` must be non-empty. The validator also
checks the shape of `root_dirs` and `import_dir`; `config validate`, service
startup, and hot-reload candidates use the same checks and name the invalid
field. See the [Configuration Reference](../configuration.md) for the full
rules. These settings require a restart, and changes affect only a database
that has not yet been initialized. Existing trees are not rebuilt.

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
| `third-party/` | Root for imported repositories. The default config aligns it with `import_dir`; validation requires the first component of `import_dir` to be listed in `root_dirs`. |
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

The directories listed in `root_dirs` (`third-party/` through `toolchains/` above; not `.cedar/` or the Buck files) are also the runtime allow-list for creating paths: a new path (a first Git push, provisioning, a product write) can only be created under one of them, and a path outside them gets `MONO_PATH_NOT_ALLOWED`; paths under `import_dir` are ImportRepos and follow their own rules. The steps and error codes for creating a new path under an existing root, and how to add a root after initialization, are in the User Guide's ["Monorepo path policy and first use"](../user-guide.md#25-monorepo-path-policy-and-first-use).

### How `import_dir` classifies paths

The actual tree always follows the running config's `root_dirs`. The sample
and `MonoConfig::default` do not necessarily include all eight suggested
directories. Set the desired list before the first initialization.

The protocol classifies each path by whether it falls under the configured
`import_dir` prefix, regardless of whether that directory already exists in
the tree. The directory itself comes from `root_dirs`; `import_dir` does not
create it. Validation requires its first component to appear in `root_dirs`,
so an inconsistent layout fails before startup. Paths under `import_dir`
always use ImportRepo semantics, even when `root_dirs` contains the same
name. For branch and tag behavior, see the [User Guide](../user-guide.md).

## 5. Configure the layout safely

Before the first service startup:

1. Set `[monorepo].root_dirs` in `config/config.toml`.
2. Ensure the first component of `import_dir` is listed in `root_dirs`; for the default `/third-party`, include `third-party`.
3. Set `admin` to the system administrator accounts used by the deployment.
4. Validate the config:

   ```bash
   cargo run -p mega2 -- --config config/config.toml config validate
   ```

5. Start the service against a new or uninitialized database. Verify the root
   with `git ls-remote` or clone it and inspect it with `git ls-tree`.

### Directory depth: top-level entries only

Each `root_dirs` string is written as one literal tree-entry name; mega2 does
not split entries on `/` to create nested directories. The validator rejects
entries containing `/`, `\`, or NUL; empty entries, `.` and `..`; leading or
trailing spaces; duplicates; and reserved root names (`.cedar`,
`.mega_cedar.json`, `.buckroot`, `.buckconfig`, and `.git`, case-insensitive).
It also requires `import_dir` to be a canonical absolute non-root path without
a trailing slash, `//`, `.` or `..` segments, NUL, or `\`. Its first
component must be in `root_dirs`; for example, `root_dirs = ["project"]` with
`import_dir = "/vendor"` is rejected. The same checks run during config
validation, service startup, and hot reload, and errors identify the invalid
field. See the [Configuration Reference](../configuration.md).

To create nested directories, initialize only the top-level layout, then
clone and add deeper paths with a regular Git commit, such as
`mkdir -p a/b && git add && git commit && git push`. After initialization,
mega2 does not rebuild the root tree from config; change the layout through
normal client commits, or reset the data plane and initialize again (a
destructive operation whose steps are outside this manual).
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
