English · [中文](contributing.zh.md)

# Contributing Guide

This guide explains how to contribute code to mega2: how to propose a change,
prepare the development environment, run the required checks, and follow the
repository's conventions. The current checkout and [`../AGENTS.md`](../AGENTS.md)
are the sources of truth. If this guide conflicts with either, follow the
source and open an issue to correct the documentation.

## 1. Contribution process

For a large change, agree on the scope and approach with the maintainers before
implementation:

1. **Open an Issue first.** State the problem, motivation, scope, and explicit
   non-goals. Wait until maintainers (or the discussion) accept the direction.
2. **Then write a plan.** Start from the English contributor template
   [`plan/plan-template.en.md`](plan/plan-template.en.md) and save the working
   plan as `docs/plan/plan-YYYYMMDD.md`. Do not
   delete mandatory sections; write `N/A` and the reason when a section does
   not apply. The operational plan archive remains Chinese-first; follow the
   repository rules in [`../AGENTS.md`](../AGENTS.md).
3. **Implement only after the plan is reviewed.** Split the work into
   independently executable task cards (clear scope / dependencies / file
   targets / acceptance criteria / verification commands), add tests and docs,
   and pass the three gates in section 3 before merge.

**A plan is not an implementation.** When drafting a plan, verify its
assumptions against the current source, tests, config, and docs. Historical
plans and agreements in issue discussions are useful context, but they do not
replace the verification commands on a task card.

## 2. Development environment

Use the unified entry script [`../scripts/dev-test.sh`](../scripts/dev-test.sh)
to prepare the Compose data plane and run tests (`up-full` / `basic` / `full` /
`gates`, etc.). Its shared logic is in
[`../scripts/lib/mega2-it.sh`](../scripts/lib/mega2-it.sh). The test
environment template is [`../.env.test.example`](../.env.test.example);
`dev-test.sh` creates a local `.env.test` when needed, and that file must not
be committed. Use `./scripts/dev-test.sh --help` for available workflows.

## 3. The three submit gates

Every code change must pass all three gates before submit (same as
[`../AGENTS.md`](../AGENTS.md); equivalent wrapper: `./scripts/dev-test.sh
gates`):

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
source .env.test && cargo test --all
```

Requirements: fmt reports no diff (nightly toolchain, because `rustfmt.toml`
may enable unstable options); clippy exits with 0 warnings and 0 errors, with
no blanket `#[allow(...)]` bypasses; all tests pass — never force green with
`#[ignore]` or deleted asserts. If `.env.test` is missing, run
`./scripts/dev-test.sh up-full` to generate and populate the local test
environment; do not skip the `source`.

## 4. Code conventions (summary)

The full conventions are in [`../AGENTS.md`](../AGENTS.md), sections Code
Conventions and Common Pitfalls; only the most frequently tripped items are
listed here:

- **Import grouping:** `std` → external crates → `crate::`; no `use crate::*`
  wildcards in library code (fine inside `mod tests`).
- **Error types:** per module, use one of `MegaError` / `MegaResult`,
  `anyhow::Result`, or `thiserror`, matching the module's existing style;
  never mix them within the same module.
- **Logging:** use the `tracing::{info, warn, error, debug, trace}` macros,
  not `println!`; prefer structured fields.
- **DB access:** go through the `*Storage` types in `src/jupiter/storage/`;
  do not call `sea_orm` directly from API / handler code.
- **Dependencies:** justify any new `Cargo.toml` dependency (compile time /
  binary size / license); prefer reusing what is already vendored.
- **Allocator:** do not touch the `#[global_allocator]` blocks in
  `src/main.rs` unless intentionally changing allocators on a platform.
- **Comments:** sparse, English, matching the surrounding file's density.
- There is no top-level `mod vault`: import Vault types from `libvault::*`
  and `crate::contract::vault::*` (see the Pitfalls section of AGENTS.md).

## 5. Common implementation tasks

**Adding a CLI subcommand** (step details in [`../AGENTS.md`](../AGENTS.md),
"Adding a New Subcommand"):

1. Implement the command module under `src/commands/<name>.rs`.
2. Register the clap `Command` in `builtin()` in `src/commands/mod.rs`, and
   wire the executor in `builtin_exec()` (signature `fn(config: Config, args:
   &ArgMatches) -> MegaResult`).
3. Add unit tests next to the command, plus a CLI parsing test in
   `src/cli.rs::tests` mirroring the existing ones.

**Adding a DB entity / migration** (step details in
[`../AGENTS.md`](../AGENTS.md), "Adding a New DB Entity / Migration"):

1. Put the entity file at `src/callisto/<table>.rs` and register it in
   `src/callisto/mod.rs`.
2. Put the migrator under `src/jupiter/migration/` and register it in that
   module's migrator list.
3. If a new domain storage is needed, add `<domain>_storage.rs` under
   `src/jupiter/storage/` and re-export it from `storage/mod.rs`.
4. Cover it with `#[cfg(test)]` tests using
   `crate::jupiter::tests::test_db_connection` +
   `crate::jupiter::migration::apply_migrations` (example:
   `notification/dispatcher.rs::tests`).

## 6. Documentation conventions

- **Plan documents:** use the English contributor template
  [`plan/plan-template.en.md`](plan/plan-template.en.md); do not invent your
  own format. The operational plan archive remains Chinese-first.
- **Fact baseline:** documents only state what is verifiable in the current
  checkout; plan documents never claim an implementation is complete.
- **Link, don't copy:** content with an authoritative home — full config key
  tables, token values, command flag lists
  ([`../config/config.toml`](../config/config.toml),
  [`deployment.md`](deployment.md), [`user-guide.md`](user-guide.md),
  [`../scripts/dev-test.sh`](../scripts/dev-test.sh),
  [`../AGENTS.md`](../AGENTS.md)) — is
  always linked, never re-printed in a new document.
- **Bilingual docs:** English is the default file (e.g. `foo.md`); Chinese
  lives in the same-named `.zh.md` sibling (e.g. `foo.zh.md`). Keep both
  versions in sync and give them the same structure, while writing the English
  version naturally rather than translating sentence by sentence. Add a
  language-switcher line at the top, following [`../README.md`](../README.md).
- Relative links in docs must resolve to files that exist in the current
  checkout; verify each one before submitting.

## 7. Adapting upstream changes

Mega was the first-generation monorepo platform; Mega2 is the second-generation
engine built for Agent workflows, with Monorepo hosting and Agent Session
Capture as its core capabilities. Mega2 ports and refactors selected parts of
the first-generation Mega project; it is not a mirror. Before adopting an
upstream change, check that it applies to a module and behavior present in this
checkout. Compare the actual source and
tests rather than copying a list of changed files, and classify changes that
depend on upstream-only services or repository structure as out of scope.

For dependency updates, compare the resolved versions in `Cargo.lock`, inspect
the release's behavioral changes, and test any affected wire or object-identity
contracts. In particular, changes to Git object serialization can change
object IDs even when public APIs stay the same. Record the upstream revision,
the compatibility decision, and any required regression coverage in the plan
or change notes. Dependencies owned by a separate repository should be
evaluated there rather than upgraded through mega2's manifest.

## 8. Version control: Libra and the task-card release flow

This repository uses **Libra** as its VCS (not git; there is no `.git`
directory): `libra add` / `libra commit` / `libra push`, etc. Interactive
browsing of the monorepo is done via Libra's `libra mega2 browser`.

Task-card release flow (details in [`../AGENTS.md`](../AGENTS.md), "Task card
release"):

1. Once a task card is complete (Lifecycle=done, dual review PASS), bump
   `Cargo.toml` `version` by the card's `Version increment` (default patch
   +1) and refresh the `mega2` entry in `Cargo.lock`.
2. `libra add` + `libra commit -m` — commit **that card only**.
3. `libra push origin main`. **Never `--force`**; if the branch has diverged
   from origin, stop and report instead of resolving it yourself.
4. Start the next card only after the previous card's commit and push succeed.

## Related documents

- [`README.md`](README.md) — index of user, operator, and developer guides
- This documentation set: [`quick-start.md`](quick-start.md) ·
  [`user-guide.md`](user-guide.md) · [`configuration.md`](configuration.md) ·
  [`deployment.md`](deployment.md) · [`architecture.md`](architecture.md)
- [`../AGENTS.md`](../AGENTS.md) — authoritative home for gates, code
  conventions, common pitfalls, and the task-card release flow
- [`../scripts/dev-test.sh`](../scripts/dev-test.sh) — local development and testing entry point
- [`plan/plan-template.en.md`](plan/plan-template.en.md) — English plan template
- [`../README.md`](../README.md) — project overview (its Contributing section
  is the English summary of this document)
