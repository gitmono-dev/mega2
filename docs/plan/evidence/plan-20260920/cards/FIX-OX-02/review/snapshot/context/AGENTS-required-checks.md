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
