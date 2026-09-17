# AGENTS.md — mega2

Guidance for AI coding agents working in this repository. Keep changes minimal,
follow the patterns already in the codebase, and verify with the commands below
before submitting.

## Project Overview

- **Name:** `mega2` (single Cargo package: lib `mega2_core` + binaries, see `Cargo.toml`).
- **Edition:** Rust 2024.
- **Purpose:** Mono‑repo / Git hosting + service engine. Ports and extends
  several subsystems originally from the Mega project (notably `callisto`
  entities and `jupiter` storage/migration).
- **Entry point:** `src/main.rs` → `cli::parse(None)`.
- **Config:** TOML loaded from `config/config.toml` (override via `--config`
  flag or `MEGA_CONFIG` env var). Loader lives in
  `src/common/config/loader.rs`.

## Tech Stack

- **Language:** Rust 2024 (stable toolchain).
- **CLI:** `clap` v4 (derive + builder), subcommands registered in
  `src/commands/mod.rs` (`builtin()` / `builtin_exec()`).
- **Async runtime:** `tokio` (full features).
- **HTTP / API:** `axum` 0.8 + `tower-http`, OpenAPI via `utoipa` + Swagger UI.
- **Storage / DB:** `sea-orm` 1.1 (Postgres + SQLite, `runtime-tokio-rustls`)
  and `sea-orm-migration`. Entities are in `src/callisto/`.
- **Cache / queue:** `redis` (with `connection-manager`).
- **Auth / policy:** `cedar-policy` (schema in `src/mega.cedarschema`,
  policies in `src/mega_policies.cedar`).
- **Crypto / TLS:** `rustls`, `ring`, `openssl`, `ed25519-dalek`, `rsa`,
  `secp256k1`, `pgp`. Vault‑style PKI/secret engine via the `libvault` crate
  (crates.io `0.3.0`, features `storage_pg` + `crypto_adaptor_openssl`) and the
  mega2 integration layer (`src/contract/vault/`). The RustyVault sources
  used to be vendored under `src/vault/`; that module was removed on 2026-08-21
  (`docs/plan/plan-20260820.md`), so import library types from `libvault::*`.
- **Email:** `lettre` (rustls + tokio).
- **Object storage:** inlined `src/orbit_api/` (traits/config) and `src/orbit/`
  (object_store backends). Built via `crate::orbit::factory::ObjectStorageFactory`
  from `src/jupiter/storage/object_storage.rs::build_object_storage`.
- **Allocator:** `jemalloc` on non‑Windows, `mimalloc` on Windows
  (configured in `src/main.rs`).
- **Logging:** `tracing` + `tracing-subscriber` + `tracing-appender`
  (hourly rolling file under `mega_cache()/logs`, or stdout when
  `log.print_std = true`).

## Commands

Run these from the repo root (the agent's shell already starts there).

| Task                 | Command                                                            |
| -------------------- | ------------------------------------------------------------------ |
| Build (release-ish)  | `cargo build`                                                      |
| Build incl. tests    | `cargo build --tests`                                              |
| Run all tests        | `cargo test`                                                       |
| Run one test         | `cargo test --test <name>` or `cargo test <substring> -- --nocapture` |
| Format               | `cargo fmt --all`                                                  |
| Lint                 | `cargo clippy --all-targets -- -D warnings` (when used)            |
| Run the binary       | `cargo run -p mega2 -- --config config/config.toml <subcommand>`            |
| HTTP service example | `cargo run -p mega2 -- --config config/config.toml service http --host 0.0.0.0 -p 9000` |

**Invariants the build must hold (verified in prior sessions):**

- `cargo build` MUST produce **0 errors and 0 warnings**.
- `cargo build --tests` MUST produce **0 errors and 0 warnings**.
- Never silence warnings by adding broad `#[allow(...)]` on items you just
  touched without a reason — the crate‑level `#![allow(dead_code)]` in
  `src/lib.rs` is intentional (large pub API surface ported from Mega);
  do not narrow or remove it without a plan to clean the dead items.

## Required Checks Before Submitting Code Changes

**Any change to the codebase MUST satisfy all three of the following gates.**
These are not optional — do not submit a change until each one is green.
Use the exact commands below (do not substitute simpler variants such as
`cargo fmt --all --check` or `cargo clippy -- -D warnings`):

1. **Formatting (nightly, check‑only):**
   ```bash
   cargo +nightly fmt --all --check
   ```
   Must report **no diff**. If it does, run `cargo +nightly fmt --all` to
   apply formatting and re‑run the check until clean. The nightly toolchain
   is required because `rustfmt.toml` may enable unstable options.

2. **Lints (all targets, all features, warnings denied):**
   ```bash
   cargo clippy --all-targets --all-features -- -D warnings
   ```
   Must exit with **0 warnings, 0 errors**. Do not bypass a clippy lint
   with a blanket `#[allow(...)]`; prefer fixing the underlying code.
   When an allow is genuinely required (e.g. a deliberate API name that
   trips `clippy::wrong_self_convention`), scope it to the smallest
   possible item and add a brief rationale.

3. **Tests (with project test env loaded):**
   ```bash
   source .env.test && cargo test --all
   ```
   Must finish with **all tests passing** (0 failures, 0 errored). The
   `.env.test` file provides DB / cache / service endpoints the test
   suite expects; do not skip sourcing it. Do not weaken or `#[ignore]`
   tests to make this gate pass — fix the root cause.

If `.env.test` is missing in your environment, stop and ask before
submitting; do not silently fall back to running `cargo test --all`
without it.

## Project Layout

```
Cargo.toml                # package `mega2` (lib `mega2_core` + [[bin]])
config/config.toml        # default runtime config (TOML)
src/
├── main.rs               # `mega2` binary entry (allocator + CLI dispatch)
├── lib.rs                # library root; declares top-level modules
├── cli.rs                # clap parsing, log init, ctrlc handler
├── orbit_api/            # object-storage contract (traits, config, errors)
├── orbit/                # object_store backends (adapter, factory)
├── bin/                  # auxiliary binaries (e.g. migrate_local_to_s3)
├── commands/             # subcommand registry (builtin / builtin_exec)
├── common/               # config loader, error types (MegaError/MegaResult), utils
├── api/                  # axum HTTP API surface
├── api_model/            # request/response DTOs (utoipa schemas)
├── server/               # HTTP/SSH/etc. server bootstrap
├── callisto/             # sea-orm entity models (one file per table)
├── jupiter/              # storage, service, migration, redis, utils
│   ├── storage/          # *Storage structs (BaseStorage + per-domain)
│   ├── migration/        # sea-orm-migration migrators
│   ├── service/
│   ├── redis/
│   └── tests.rs          # `pub mod tests` (cfg(test)) — shared test helpers
├── notification/         # email notifications: dispatcher, triggers, storage
├── email/                # Mailer trait + impls (incl. NoopMailer)
├── contract/
│   └── vault/            # PKI / KV / secret engine integration layer over the
│       │                 # `libvault` crate (no vendored module since 2026-08-21)
│       └── integration/
│           ├── jupiter_backend.rs
│           └── vault_core.rs # VaultCore, VaultCoreInterface
├── ceres/  context/
└── mega.cedarschema, mega_policies.cedar
tests/                    # process-level integration tests (integration_*.rs)
target/                   # build artifacts (gitignored)
```

`pub use crate::callisto::*;` is re‑exported from `lib.rs`; importing
`callisto` entities elsewhere should use `crate::callisto::<table>` paths.
Object storage public types are available from `crate::orbit_api::*`; the
concrete backend is built through `crate::jupiter::storage::object_storage::build_object_storage`
(which calls `crate::orbit::factory::ObjectStorageFactory::build`).

## Code Conventions

- **Formatting:** match `cargo +nightly fmt --all` output (the same
  formatter used by the required `cargo +nightly fmt --all --check` gate).
  Don't hand‑format around it.
- **Imports:** group by `std` → external crates → `crate::` (matches the
  existing files). Avoid wildcard `use crate::*;` in library code; wildcards
  are fine inside `mod tests`.
- **Errors:** use `crate::common::errors::{MegaError, MegaResult}` for
  application code paths that already use them; `anyhow::Result` is used in
  lower‑level utilities and `thiserror` for new typed errors. Don't mix the
  three within the same module.
- **Async:** functions returning `Result` should be `async fn -> Result<T, E>`
  using `tokio` runtime. Don't add `block_on` inside async contexts.
- **Logging:** use `tracing::{info, warn, error, debug, trace}` macros, not
  `println!`. Structured fields preferred (e.g. `info!(path = %p, "loaded")`).
- **DB access:** go through the `*Storage` types in `src/jupiter/storage/`
  rather than calling `sea_orm` directly from API/handler code.
- **Comments:** sparse, English. Match the surrounding density — do not add
  comments to files that don't already use them.
- **Files / modules:** snake_case filenames, one module per file, `mod.rs`
  only for directory module roots.

## Common Pitfalls (please read before editing tests or `vault`)

1. **`sea_orm` imports inside tests.** There is no `crate::jupiter::sea_orm`
   re‑export. Import traits from the top‑level crate:
   ```rust
   use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
   ```
   Forgetting these traits produces misleading errors such as
   `email_jobs::Entity is not an iterator`.
2. **`VaultCore` path.** `src/contract/vault/integration/mod.rs` does **not**
   re‑export `VaultCore`. Import it directly from its submodule:
   ```rust
   use crate::contract::vault::integration::vault_core::{VaultCore, VaultCoreInterface};
   ```
3. **Glob re‑exports / the `vault` name.** There is no top-level `mod vault;`
   any more — the vendored RustyVault module was removed on 2026-08-21 and the
   library comes from the `libvault` crate. `crate::vault::*` is not a valid
   path; `rg 'crate::vault' src bin` should stay at zero hits. Three different
   things still share the name, so keep imports explicit: `crate::callisto::vault`
   is the SeaORM entity, `crate::contract::vault` is the product integration
   layer, and `libvault::*` is the library itself.
4. **Crate‑level `dead_code` allow.** `#![allow(dead_code)]` in `main.rs`
   is intentional. If you add a new pub API item, you don't need to add
   per‑item allows; if you remove the crate‑level allow, expect ~70 warnings.
5. **Test DB helpers.** Tests requiring a database use
   `crate::jupiter::tests::test_db_connection(<TempDir path>)` followed by
   `crate::jupiter::migration::apply_migrations(&db, true).await`. Reuse
   these helpers instead of constructing connections by hand.
6. **Allocator cfg.** Don't touch the `#[global_allocator]` blocks in
   `main.rs` unless intentionally changing allocators on a platform.
7. **`unwrap()` in non‑test code.** Avoid introducing new `unwrap`/`expect`
   on fallible operations; return `MegaResult`/`anyhow::Result` instead.
   Existing call sites in `vault/pki.rs` test helpers are OK because they
   are test‑only.

## Adding a New Subcommand

1. Implement the command module under `src/commands/<name>.rs`.
2. Register it in `src/commands/mod.rs` via `builtin()` (clap `Command`) and
   wire its executor in `builtin_exec()` so `cli::exec_subcommand` finds it.
3. The executor signature is `fn(config: Config, args: &ArgMatches) -> MegaResult`.
4. Add unit tests next to the command and, where useful, a CLI parsing test
   mirroring the existing ones in `src/cli.rs::tests`.

## Adding a New DB Entity / Migration

1. Generate or hand‑write the entity file under `src/callisto/<table>.rs`
   and add it to `src/callisto/mod.rs`.
2. Add a migrator under `src/jupiter/migration/` and register it in that
   module's migrator list.
3. If a new domain storage is needed, add `<domain>_storage.rs` under
   `src/jupiter/storage/` and re‑export from `storage/mod.rs`.
4. Cover with a `#[cfg(test)] mod tests` that uses `test_db_connection` +
   `apply_migrations` as shown in `notification/dispatcher.rs::tests`.

## Website-next IT stack reload

When you change **`../megaui`** sources that affect `apps/web`, rebuild and
restart the compose `website-next` service after
the change (do not wait for the user to ask):

```bash
./scripts/reload-website-next.sh
```

The script debounces rapid edits (~3s) and runs `docker compose` build + recreate
in the background. Logs: `${TMPDIR:-/tmp}/mega2-reload-website-next/build.log`.

Project hooks in `.cursor/hooks.json` trigger the same script on megaui file
edits and again on agent `stop` when a reload was requested.

## Task card release (plan-20260905 and other `docs/plan/` cards)

When a plan task card is complete (`Lifecycle=done`, dual review PASS), do
**not** wait for the user to ask: bump version, commit that card only, and
push. VCS is Libra (no `git`).

1. Bump `Cargo.toml` `version` by the card’s `Version increment` (default
   **patch +1**) and refresh `Cargo.lock` for `mega2`.
2. `libra add` + `libra commit -m` for **that card only**.
3. `libra push origin main`. Never `--force`. If the branch has diverged from
   origin, stop and report.
4. Start the next card only after this card’s commit and push succeed.

See `.cursor/rules/task-card-release.mdc`.

## Boundaries

- **Do not** commit secrets, real tokens, or production `config.toml` values.
- **Do not** add new dependencies to `Cargo.toml` without confirming they
  pull their weight (compile time / binary size / license). Prefer reusing
  what's already vendored (e.g. `reqwest`, `rustls`, `tokio`).
- **Do not** rewrite working modules to "modernize" them; keep diffs focused
  on the requested change.
- **Do not** disable or weaken tests (`#[ignore]`, `--skip`, deleted asserts)
  to make a build pass. Fix the root cause or ask.
- **Do** run `cargo build` and `cargo build --tests` before submitting any
  change that touches `src/`.

## Verification Checklist (before submit)

**Required gates (see _Required Checks Before Submitting Code Changes_):**

- [ ] `cargo +nightly fmt --all --check` → no diff.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` → 0 warnings, 0 errors.
- [ ] `source .env.test && cargo test --all` → all tests pass.

**Additional sanity checks:**

- [ ] `cargo build` → 0 errors, 0 warnings.
- [ ] `cargo build --tests` → 0 errors, 0 warnings.
- [ ] No stray debug files (`.warnings.log`, `.output.txt`, ad‑hoc scripts)
      left in the repo root.
- [ ] No new top‑level `#[allow(...)]` other than what already exists.
