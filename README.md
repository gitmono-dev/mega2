# monoengine

> A Rust-based monorepo / Git hosting and service engine. Ports and extends several subsystems originally from the [Mega](https://github.com/web3infra-foundation/mega) project (notably the `callisto` ORM entities and the `jupiter` storage / migration layer) into a focused binary crate.

`monoengine` is the backend engine that powers a monorepo platform: it speaks the Git wire protocols over HTTP(S) and SSH, persists Git objects and Git LFS blobs into a relational database plus pluggable object storage, exposes a REST/OpenAPI surface for higher‑level UI clients, and ships an embedded vendored `libvault` secret / PKI engine for signing and credential management.

---

## Highlights

- **Cargo workspace** — `monoengine-core` (lib, depends only on `orbit-api`)
  + `monoengine` (thin binary, injects the `orbit` implementation crate);
  `cargo build -p monoengine` produces the `monoengine` executable.
- **Git hosting** — HTTP(S) and SSH Git transport (`receive-pack` / `upload-pack`),
  smart‑HTTP discovery, and Git‑LFS (basic, multipart, optional SSH transport).
- **Monorepo semantics** — first‑class concept of an "import dir" (multi‑branch,
  third‑party mirrors) vs. the monorepo tree (single main branch, structured
  root dirs like `project/ doc/ release/ model/ toolchains/`).
- **Change Lists & merge checks** — CL (PR‑equivalent) lifecycle with pluggable
  checkers: GPG signature, branch protection, commit‑message style, CL sync,
  merge conflicts, CI status, code review, CLA sign.
- **Build trigger pipeline** — webhook / manual / scheduled / retry / web‑edit /
  Buck‑file‑upload triggers, dispatched to an external Orion build server.
- **OpenAPI + Swagger UI** — every HTTP route documented via `utoipa`, browsable
  out of the box.
- **Pluggable storage** — `sea-orm` against PostgreSQL or SQLite for metadata;
  `orbit-api` provides the shared object-storage traits/config, while `../orbit`
  supplies the concrete `object_store` adapter for
  blobs/LFS on **local FS, AWS S3, S3-compatible (RustFS, MinIO, ...), or GCS**;
  `redis` for cache / queue.
- **Email notifications** — async dispatcher backed by an `email_jobs` queue,
  SMTP via `lettre` (rustls + tokio), event triggers for CL comments, and
  admin-managed plus configurable automatic outbox attachment cleanup.
- **Embedded Vault** — PKI (root CA, role‑based cert issuance) and a KV / secret
  engine via the vendored RustyVault module in `src/vault`, with a `jupiter`
  storage backend so the vault lives in the same database.
- **Cedar‑policy authorization** — schema (`src/mega.cedarschema`) and policies
  (`src/mega_policies.cedar`) shipped alongside the binary.
- **Production allocators** — `jemalloc` on Unix, `mimalloc` on Windows.
- **Structured logging** — `tracing` with hourly‑rolling file appender or
  stdout, configurable ANSI colours and log level.

---

## Tech Stack

| Layer            | Crates / Tech                                                                 |
| ---------------- | ----------------------------------------------------------------------------- |
| Language         | Rust **2024 edition** (stable toolchain; nightly used only for `rustfmt`)     |
| CLI              | `clap` v4 (derive + builder)                                                  |
| Async runtime    | `tokio` (full)                                                                |
| HTTP             | `axum` 0.8, `tower-http`, `tower-sessions`                                    |
| API docs         | `utoipa` + `utoipa-swagger-ui`                                                |
| ORM / DB         | `sea-orm` 1.1 (Postgres + SQLite, `runtime-tokio-rustls`) + `sea-orm-migration` |
| Cache / queue    | `redis` (`aio`, `tokio-rustls-comp`, `connection-manager`)                    |
| Object storage   | `orbit-api` interface + sibling `../orbit` `object_store` adapter             |
| SSH              | `russh`                                                                       |
| Auth / policy    | `cedar-policy`                                                                |
| Vault / PKI      | vendored RustyVault module in `src/vault`                                     |
| Crypto / TLS     | `rustls`, `ring`, `openssl`, `ed25519-dalek`, `rsa`, `secp256k1`, `pgp`       |
| Email            | `lettre` (rustls + tokio)                                                     |
| Logging          | `tracing`, `tracing-subscriber`, `tracing-appender`                           |
| Allocator        | `jemallocator` (Unix) / `mimalloc` (Windows)                                  |

The full dependency list lives in `Cargo.toml`.

---

## Quick Start

### Prerequisites

- Rust toolchain (stable) — recommended via [rustup](https://rustup.rs).
  Nightly is only required for running the formatter check.
- PostgreSQL **or** SQLite (the default `config/config.toml` is wired for
  PostgreSQL; switch `database.db_type` to `"sqlite"` for a zero‑setup demo).
- (Optional) Redis, if you exercise paths that hit the cache / queue.
- (Optional) An SMTP endpoint if you want real email delivery; otherwise the
  `NoopMailer` is used.

### Build

```bash
# Debug build (workspace default = monoengine-core lib only)
cargo build

# Build the monoengine binary explicitly
cargo build -p monoengine

# Build including tests (must stay at 0 warnings / 0 errors — see AGENTS.md)
cargo build --tests
```

### Configure

The runtime is driven by a TOML config file. The default ships at
`config/config.toml` and supports `${base_dir}` interpolation plus environment
overrides.

Resolution order:

1. `--config <path>` CLI flag.
2. `MEGA_CONFIG` environment variable.
3. The bundled `config/config.toml`.
4. `${mega_base}/etc/config.toml`.
5. If no config exists, generate the default config and load it.

When `--profile <name>` or `MEGA_PROFILE=<name>` is set, monoengine also
loads `config.<name>.toml` next to the selected base config. CLI profile wins
over `MEGA_PROFILE`, and the final merge order is base config, profile config,
then `MEGA_*` environment overrides.

The bundled config is a local sample and does not embed reusable database
passwords. Inject real bootstrap credentials through `MEGA_DATABASE__DB_URL`,
profile files, or your deployment secret mechanism.

Use `monoengine config init` to generate a safe starter config, and
`monoengine config validate` to check the selected base/profile/env merge
without starting services. Add `--show-sources` to print source warnings,
field sources, and overrides without values; add `--deny-warnings` to fail on
ignored or deprecated config fields.

Key sections (see `config/config.toml` for the full list):

| Section            | Purpose                                                                 |
| ------------------ | ----------------------------------------------------------------------- |
| `base_dir`         | Root for logs, caches, SQLite/LFS data (override with `MEGA_BASE_DIR`)  |
| `[log]`            | Level, stdout vs. rolling file, ANSI colours                            |
| `[database]`       | `db_type = "postgres"` or `"sqlite"`, URL, pool sizing, timeouts        |
| `[monorepo]`       | `import_dir`, admin users, default `root_dirs`, rename detection limits |
| `[pack]`           | Pack decode memory/disk budget and cache path                           |
| `[lfs]`            | LFS HTTP/SSH endpoints and local storage path                           |
| `[object_storage]` | `local` / `s3` / `s3compatible` / `gcs` backends                        |
| `[oauth]`          | CORS allow-list (`allowed_cors_origins`); consumed by the HTTP API's `CorsLayer` |
| `[redis]`          | Connection URL                                                          |
| `[build]`          | Orion build server URL and trigger preheat depth                        |
| `[buck]`           | Buck upload session limits, cleanup schedule, and concurrency caps       |
| `[artifacts_gc]`   | Background GC enable flag and schedule for orphan repo artifact blobs    |
| `[mail]`           | SMTP settings, `password_ref`, dispatcher limits and retry policy         |
| `[sidebar]`        | Default UI sidebar items seeded into a fresh DB                         |

`mail.password_ref` is the first config-backed monoengine Vault SecretRef.
It must use `vault://secret/config/<profile>/mail/password#<field>`.
Redis URL (`redis.url`), object storage S3 credentials
(`object_storage.s3.access_key_id` / `secret_access_key`), and notification
channel credentials (`notification.slack.webhook_url_ref`,
`notification.webhook.token_ref`) also support `vault://` SecretRefs, each
with its own required namespace. Database credentials remain
deployment/env secrets (they are consumed before the vault is ready).
`config validate` and `config secret set/check` enforce the correct
namespace for each supported field; `config validate --resolve-secrets`
resolves all supported SecretRefs through the minimal DB/Vault bootstrap.

### Run

```bash
# Start the HTTP server
cargo run -p monoengine -- --config config/config.toml service http --host 0.0.0.0 -p 9000

# Start the SSH Git server
cargo run -p monoengine -- --config config/config.toml service ssh

# Start multiple services in the same process (HTTP is mandatory)
cargo run -p monoengine -- --config config/config.toml service multi http ssh
```

On first boot against an empty database the embedded `sea-orm-migration`
migrators (under `src/jupiter/migration/`) will apply the schema and seed the
default sidebar / event types.

---

## CLI Overview

```
monoengine [--config <file>] [--profile <name>] <SUBCOMMAND>
```

| Subcommand              | What it does                                                       |
| ----------------------- | ------------------------------------------------------------------ |
| `service http`          | Start the axum HTTP server (Git smart‑HTTP + REST API + Swagger UI) |
| `service ssh`           | Start the `russh`‑based SSH Git transport server                   |
| `service multi <kinds…>` | Start several servers in one process (e.g. `multi http ssh`)       |

CLI options come from `clap` derive types and respect `--help` at every level:

```bash
cargo run -p monoengine -- --help
cargo run -p monoengine -- service --help
cargo run -p monoengine -- service http --help
```

---

## Project Layout

```
Cargo.toml                # workspace root + monoengine-core lib manifest
bin/                      # thin composition-root binary crate (monoengine)
├── Cargo.toml            # binary manifest; depends on monoengine-core + orbit
├── src/main.rs           # binary entry; registers ObjectStorageProvider
└── tests/                # black-box integration tests (integration_vault.rs)
config/config.toml        # default runtime config (TOML)
rustfmt.toml              # nightly-only formatter options
src/
├── lib.rs                # library entry; declares all top-level modules
├── cli.rs                # clap parsing, log init, ctrlc handler
├── commands/             # subcommand registry (service / http / ssh / multi)
├── common/               # error types (MegaError/MegaResult), utils
├── config/               # config module: model / loader / SecretRef / reload
├── context/              # AppContext: shared state (DB, storages, services)
├── api/                  # axum HTTP API surface (routes / handlers)
├── server/               # HTTP / SSH server bootstrap
├── callisto/             # sea-orm entity models (one file per table)
├── jupiter/              # storage / service / migration / redis / utils
│   ├── storage/          # *Storage structs (BaseStorage + per-domain)
│   ├── migration/        # sea-orm-migration migrators
│   ├── service/          # higher-level service objects
│   ├── redis/            # Redis client + helpers
│   ├── utils/            # diff / reanchor / misc utilities
│   └── tests.rs          # shared test helpers (cfg(test))
├── ceres/                # CL (Change List) logic, merge checks, build triggers
├── mail/                 # first-class mail module: Mailer trait + provider factory
├── notification/         # email notification dispatcher + event triggers
├── email/                # legacy Mailer re-export shim (now re-exports crate::mail)
├── vault/                # vendored RustyVault module (crate::vault)
├── contract/             # API / Git / Vault / Policy data & boundary contracts
│   ├── api/              # request/response DTOs (utoipa schemas; was api_model)
│   ├── git_protocol/     # smart-HTTP and SSH Git wire protocol glue
│   ├── policy/           # cedar policy guard + reviewer/admin resolution (was saturn + api::guard)
│   └── vault/            # PKI + KV secret engine integration layer
│       └── integration/  # VaultCore, VaultCoreInterface, jupiter_backend
├── bellatrix/            # supporting subsystem ported from Mega
└── mega.cedarschema, mega_policies.cedar
test/project/             # fixture data for integration tests
target/                   # build artifacts (gitignored)
```

`pub use crate::callisto::*;` is re‑exported from `lib.rs`; when importing
entities elsewhere, prefer the explicit `crate::callisto::<table>` path.
Object storage public types remain available from `orbit_api::*`; the
`monoengine-core` library only depends on `orbit-api` (no heavy
`object_store` / cloud SDKs). The concrete backend is injected at the
composition root: `bin/src/main.rs` registers an `OrbitObjectStorageProvider`
that calls `orbit::factory::ObjectStorageFactory::build`, and
`AppContext::new` resolves `vault://` SecretRefs for S3 credentials after
the DB-only vault bootstrap, then constructs the object store and injects
it into `Storage::new_with_connection`.

---

## Development

### Required Gates Before Submitting Code

Every code change must pass the three commands below (these are enforced by
`AGENTS.md` and CI):

```bash
# 1. Formatting (nightly, check-only)
cargo +nightly fmt --all --check

# 2. Lints (all targets, all features, warnings denied)
cargo clippy --all-targets --all-features -- -D warnings

# 3. Tests (with the project test environment loaded)
source .env.test && cargo test --all
```

Additional sanity checks:

- `cargo build` → 0 errors, 0 warnings.
- `cargo build --tests` → 0 errors, 0 warnings.

> `.env.test` is **not** committed (it contains DB / cache / service endpoints
> for the test harness). If it is missing in your environment, stop and ask
> rather than silently running `cargo test --all` without it — several tests
> require it.

### Coding Conventions (excerpt)

- **Formatting** — match `cargo +nightly fmt --all` output; don't hand‑format.
- **Imports** — group by `std` → external crates → `crate::`; avoid wildcard
  `use crate::*;` outside `mod tests`.
- **Errors** — application paths use `crate::common::errors::{MegaError, MegaResult}`;
  low‑level utilities use `anyhow::Result`; new typed errors use `thiserror`.
  Don't mix the three within the same module.
- **Logging** — use the `tracing` macros (`info!`, `warn!`, `error!`, `debug!`,
  `trace!`), never `println!`.
- **DB access** — go through the `*Storage` types in `src/jupiter/storage/`
  rather than calling `sea_orm` directly from API/handler code.
- **`unwrap()`** — avoid in non‑test code; return `MegaResult` / `anyhow::Result`
  instead.

The complete agent contract — including common pitfalls (e.g. `sea_orm` import
paths in tests, `VaultCore` import path, the intentional crate‑level
`#![allow(dead_code)]`) and recipes for adding subcommands or DB entities —
lives in [`AGENTS.md`](AGENTS.md).

### Running a Single Test

```bash
# By test substring
cargo test test_dispatcher_sends_pending_jobs -- --nocapture

# By integration test binary name
cargo test --test <name>
```

---

## Architecture Notes

- **`AppContext`** (`src/context/`) is the per‑process shared state. It owns
  the `sea-orm` connection pool, every `*Storage` handle, the object‑store
  client, the Redis client, the mailer, and the vault. Subcommand executors
  build it once and pass `AppContext` (or clones) into route layers and
  background tasks.
- **`callisto` ↔ `jupiter`** — `callisto` is the *what* of the schema (one
  `sea-orm` entity file per table). `jupiter::storage` is the *how* (typed
  query helpers, transactions, joins). Higher‑level modules never touch
  `sea-orm` directly; they go through `jupiter::storage`.
- **CL / merge pipeline** — `ceres::merge_checker` defines a `Checker` trait
  and a `CheckType` enum (GPG signature, branch protection, commit message,
  CL sync, merge conflict, CI status, code review, CLA sign). Each check
  yields a `ConditionResult` (`PASSED`/`FAILED`) and aggregates into a
  `RequirementsState` (`MERGEABLE`/`UNMERGEABLE`).
- **Notifications** — `notification::triggers` enqueues `email_jobs` rows in
  response to CL events; `notification::dispatcher::EmailDispatcher` polls
  that queue on a 2s tick, claims jobs atomically, loads persisted
  `email_job_attachments`, and hands them to the
  `Mailer` trait (`mail::SmtpMailer`, `mail::ConsoleMailer`, `mail::http::HttpMailer`, or
  `mail::NoopMailer`). Config reload
  can disable a running dispatcher via `mail.enabled = false` and hot-reload
  dispatcher batch/concurrency limits plus retry policy; re-enabling mail or
  changing SMTP settings still requires restart. Mail templates have built-in
  locale fallback and can be overridden at startup with TOML files from
  `mail.template_dir`; admins can audit built-in/external template sources and
  preview or upsert external TOML templates under `/admin/mail-templates`.
  Users can inspect and update notification settings, preferred locale, and
  per-event preferences under `/user/notification/preferences`; admins can
  manage notification event types
  under `/admin/notification-event-types` and prune old terminal outbox jobs
  under `/admin/email-jobs/prune`; attachment audit metadata is available under
  `/admin/email-jobs/{id}/attachments`, and admins can explicitly download,
  delete, or prune old terminal-job persisted attachments under the
  `/admin/email-jobs/.../attachments` management endpoints.
- **Background maintenance** — HTTP service tasks clean expired Buck upload
  sessions and unreferenced artifact blobs. Running Buck cleanup and artifact GC
  tasks hot-reload schedule settings and can be disabled without restart;
  enabling either task from off still requires restart.
- **Config reload boundaries** — log settings, mail disable, and running
  maintenance schedules are the hot-reload surface. Static consumers such as
  database, Redis, object storage, LFS, build/orion, sidebar, pack, blame, and
  monorepo layout changes are reported as restart-required and do not publish a
  candidate snapshot.
- **Vault** — `contract::vault::integration::vault_core::VaultCore` wraps
  the vendored `crate::vault` module with a `jupiter`‑backed storage adapter
  (`contract::vault::integration::jupiter_backend`), so secrets live in the
  same Postgres / SQLite database as everything else.

---

## Contributing

Bug reports, design discussions and pull requests are welcome.

1. Read [`AGENTS.md`](AGENTS.md) — it documents the build invariants,
   required CI gates, and the project's coding conventions in full.
2. Make focused changes — don't rewrite working modules to "modernize" them,
   and don't widen `#[allow(...)]` scope or `#[ignore]` tests to make a build
   pass.
3. Sign your commits (`git commit -s -S …`) and run the three required gates
   locally before pushing.

---

## License

See repository metadata / `LICENSE` (if present) for licensing terms. As
`monoengine` ports code from the Mega project, downstream consumers should
also respect the upstream Mega licensing.
