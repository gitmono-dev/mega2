# mega2 Plan Template (English)

This is the English edition of `docs/plan/plan-template.md` for contributors. New dated plans must copy this structure (or the Chinese original), replace `<...>` placeholders, and delete unused explanatory prose. **Do not delete mandatory sections.** If a section does not apply, write `N/A` and the reason.

The Chinese file remains the in-repo operational original. If the two texts ever diverge on a gate or field, follow the Chinese file and open an Issue to sync this copy.

**Template version:** `v2` (effective 2026-07-29). This edition adds task-card granularity rules `G-*`; fields `Task type`, `Lifecycle / Acceptance`, `Out of scope`, `Implementation write set`, `Release write set`, `Rollback mode`, `Version increment`, `C/D coverage from`, `Granularity`; the dependency register; release groups and concurrency windows; revision history; and stable `ER-*` IDs.

Product name is **mega2**. The Cargo package and binaries are still `monoengine` / `monoengine_core`. mega2 inherits from the same organization's [Mega](https://github.com/web3infra-foundation/mega) project. Mega is the transplant source and contract baseline, not a competitor.

### Version and migration policy

- Plans **created after** the effective date must match this version in full.
- Plans already written before that date migrate **incrementally**: only cards you add or normatively edit must satisfy current `G-*` and new fields. Untouched cards stay as-is.
- Migrating an entire legacy plan is its own piece of work. Do not sneak it into another task.
- If a legacy plan temporarily keeps a conflicting field set, record one exception row in that plan's revision history with an expected migration time.

## Usage rules

- Name dated plans `plan-YYYYMMDD.md`. Use them for executable implementation, migration, refactor, or release work.
- Long-lived capabilities belong only in `plan-long.md` (`PT-*` and `SB-*`). Dated plans may link those IDs. Do not copy the long-term roadmap into a second task table.
- Every plan's fact baseline is the current checkout: source, tests, config, and docs. Older plans, screenshots, and meeting notes are clues only. Mega's pinned revision is the transplant baseline; Mega's historical prose is not proof of what mega2 already does.
- Every task card must be executable by one Agent alone: clear scope, clear dependencies, concrete file landings, pass/fail acceptance, and copy-paste verification commands.
- Every card must satisfy all `G-*` granularity rules: one independently recoverable behavior axis, item and size limits, one release slice per card by default. Ungranular cards must be split or merged before work starts.
- Plans that touch public commands, config keys, DB schema, HTTP APIs, error types, storage formats, the Git protocol, migrations, authz, or security boundaries must include tests, docs, rollback, and compatibility handling.
- If you cite Mega or another external repo (Libra, upstream orbit, …), pin a revision, file path, and check date. Do not treat a floating `main` as a spec.
- New or changed entity / storage / migration work must update `src/callisto/`, `src/jupiter/storage/`, and `src/jupiter/migration/` (including the `migrations()` list in `src/jupiter/migration/mod.rs`) and add matching integration tests.
- Confirm every `docs/*.md` path exists before you cite it. New plans must not add dangling links: create the doc in the same card, or cite a file that already exists.
- Production code must not add unexplained `unwrap()`, `expect()`, or `panic!()`. If the path is truly infallible, add an `// INVARIANT:` comment and say so in the card's acceptance.

### Normative IDs and terms

Cite rules by ID (`G-03`, `ER-04`). Do not write "the previous section" or other references that break when clauses move. New clauses must be added to this table.

| Prefix | Meaning | Defined in |
|---|---|---|
| `ER-*` | Execution requirements (start, accept, release, evidence) | Execution requirements |
| `GC-*` | Global engineering constraints (every task) | Global engineering constraints |
| `G-*` | Task-card granularity | Task-card granularity rules |
| `ADR-*` | Accepted design decisions | Accepted design decisions |
| `GAP-*` | Fact-baseline gaps | Current gaps |
| `DEP-*` | Dependencies (cross-plan and external) | Dependency register |
| `REL-*` | Release groups (including family-card windows) | Release groups and concurrency windows |
| `EX-*` | Allowlisted waivers (named approval) | Field defaults and exceptions |
| `FIX-*` | Out-of-scope fix cards found during execution (ER-10) | End of the owning Phase |
| `DEFER-*` | Deferred items (`DEFER-<plan-prefix>-NN`) | Non-goals and deferred items |
| `M<n>` | Milestones (`M0`, `M1`, …; no hyphen) | Milestone acceptance and rollback |

IDs this template must not reuse: `plan-long.md` `PT-01..PT-13` (from `PT-13`, items may be mega2-native) and `SB-01..SB-03`; each dated plan's own task prefixes (for example `IT-*` in `plan-20260727.md`).

Terms (use these; do not invent synonyms):

- **Behavior axis:** one independently recoverable change with self-contained external semantics. "LFS batch auth" is one axis; "LFS content addressing" is another.
- **Landing:** one enumerable code or doc ownership domain: a **concrete directory** (`src/jupiter/storage/`, `src/api/router/`) or a **same-topic doc set** (`docs/refactoring/config.md`). Repo root, `src/`, `tests/`, and `docs/` are **not** one landing.
- **Write set:** files that will change, in three classes (G-10): **implementation write set I** (per card; decides concurrency), **release write set R** (per card; version face + `Cargo.lock`; today the version face is only `Cargo.toml` `version`; see ER-08; not used to group implementation concurrency, but serialized under I–R / R–R once a release window opens), **coordination write set C** (plan-level release order and windows; not a card field; "no concurrent multi-Agent release" is ER-12).
- **Release slice:** one independent review + acceptance + version + commit + push.
- **Family cards:** a set of child cards that share one unique release point (G-08).
- **Rollback mode:** exactly one of `revert` / `forward-only` / `compensating` / `immutable-release` (G-01). Irreversible work uses the last three; do not demand "one revert undoes it".

## Title

`# <topic> plan (<YYYY-MM-DD>)`

## Document duty

This document addresses `<problem / capability>`. The deliverable is `<result>`.

This file only plans work. It does not claim the work is done. At execution time, refresh source anchors first, then accept the card.

### Scope

- `<commands / modules / services>`
- `<DB schema, HTTP API, config keys, or storage formats>`
- `<tests, docs, migrations, or release actions>`

### Non-goals

- `<capability we will not build>`
- `<scope deferred to another plan / RFC / ADR>`
- `<behavior that is easy to assume but is not promised>`

### Success

- `<user or system behavior change>`
- `<machine-interface or data-state change>`
- `<docs, tests, release evidence>`
- `<when this plan may be marked complete>`

## Fact baseline

> Refresh every line number and source anchor on the day work starts. Stale anchors are historical clues only.

| Category | Current fact | Evidence |
|---|---|---|
| Code entry | `<src/...>` | `<file:line>` |
| Data / state | `<Postgres table / redis key / object namespace>` | `<file:line>` |
| CLI | `<monoengine ...>` | `<src/commands/mod.rs:line (builtin / builtin_exec / load_mode)>` |
| HTTP API | `<METHOD /api/v1/...>` | `<src/api/router/...:line>` |
| Config | `[section].key` | `<config/config.toml:line + src/config model:line>` |
| Errors | `<MegaError::...>` | `<src/common/errors/mod.rs:line>` |
| Migration | `<m<YYYYMMDD>_<HHMMSS>_<slug>>` | `<migrations() registration line>` |
| Docs | `<docs/...>` | `<file:line>` |
| Tests | `<-p monoengine --lib '<mod::tests>' or --test <target>>` | `<file:line>` |
| Workspace prelude | `<.env.test present / test stack up (Postgres 15432, Redis 16379, …)>` | `<.env.test.example / docker-compose.test.yml:line>` |
| External reference | `<Mega repo@sha>` | `<path + check date>` |

### Current gaps

| ID | Gap | Impact | Evidence | Plan action |
|---|---|---|---|---|
| GAP-01 | `<problem>` | `<user / production impact>` | `<file:line or external evidence>` | `<task ID>` |

## Relation to other plans

| Plan / doc | Relation | This plan does |
|---|---|---|
| `plan-long.md` | `<PT / SB IDs>` | `<link, consume, update status, or leave untouched>` |
| `plan-YYYYMMDD.md` | `<blocked-by / parallel / supersede / conflict>` | `<reuse, do not redo, migrate, close>` |
| `docs/refactoring/*.md` | `<fact source or contract>` | `<how we sync>` |
| `AGENTS.md` / `README.md` | `<engineering baseline>` | `<obey, propose a revision, or register drift>` |

## Review conclusions and revision record

Self-review these dimensions before the plan is final. Fix blockers before any card starts.

| Dimension | Conclusion | Revision |
|---|---|---|
| Worth doing | `<is the goal worth the cost>` | `<adjust>` |
| Feasibility | `<can tasks be split and delivered>` | `<adjust>` |
| Granularity | `<multi-axis, L/XL, fragment, unregistered merged release>` | `<split / merge / register exception per G-*>` |
| Dependencies and order | `<DAG acyclic, missing edges, executable release order>` | `<adjust>` |
| Completeness | `<tests / docs / migrations / rollback>` | `<adjust>` |
| Security | `<authz, secrets, paths, network, model input>` | `<adjust>` |
| Functional correctness | `<state machines, edges, error paths>` | `<adjust>` |
| Interface compatibility | `<CLI / HTTP / config / schema / errors>` | `<adjust>` |
| Data and control flow | `<transactions, idempotency, concurrency, distributed state>` | `<adjust>` |
| Performance and capacity | `<hot paths, complexity, storage growth>` | `<adjust>` |
| Reliability | `<crash recovery, retry, resource release>` | `<adjust>` |
| Maintainability | `<fact sources, boundaries, duplicated logic>` | `<adjust>` |

### Revision history

Every normative change after the plan is written (split/merge, dependency change, release-boundary change, decision reversal) gets one row. G-09 split sync closes here.

| Date | Trigger | Change | Old card → new cards | Affected references |
|---|---|---|---|---|
| `<YYYY-MM-DD>` | `<self-review / review R<n> / baseline check>` | `<what changed>` | `<TASK-ID> → <TASK-ID>, <TASK-ID>` | `<order, DEP, REL-*, traceability, test matrix, milestones, risks>` |

## Accepted design decisions

If implementation must deviate, edit the plan first. Do not silently change semantics in code.

### ADR-<PREFIX>-01: <title>

- **Status:** Accepted
- **Context:** `<why this decision exists>`
- **Decision:** `<chosen option>`
- **Alternatives considered:** `<rejected options and why>`
- **Consequences:** `<constraints, risks, follow-up>`
- **Revisit when:** `<when to reopen>`

## Global engineering constraints

These apply to every task in the plan. Cards do not repeat them. Breaking any item means the task is not done.

- **GC-01 Verify before you build:** On start, re-check the plan, related docs, current code, and tests. If the work is already done, add tests, add docs, update status, or close the card. Do not re-implement it.
- **GC-02 Single source of truth:** Entity definitions, config parsing, API schemas, authz policy, error types, and shared helpers have one source. CLI handlers, HTTP handlers, migrations, and test fixtures must not each copy equivalent logic.
- **GC-03 Mega vs mega2 extension boundary:** Code transplanted from Mega must mark source and diff. mega2-only surfaces must state the alternative, user impact, and machine interface.
- **GC-04 Output and error contract:** User-visible errors use stable `MegaError` variants and stay in sync with `docs/errors.md`. HTTP status, JSON body, CLI exit code, and human-readable output are accepted separately. `MegaError` has **no** numeric error-code registry; variant → HTTP status lives only in `src/common/errors/api.rs`. Changing that map updates `docs/errors.md` or the card explains why not.
- **GC-05 Doc sync:** Command, config, HTTP API, or public-behavior changes update the matching `docs/` file, `config/config.toml` comments, and `README.md`. `docs/` is Chinese-only; there is no EN/zh pair requirement. OpenAPI is aggregated at runtime by `utoipa`; **there is no on-disk spec file**. API schema evidence comes from a running `/api/openapi.json`.
- **GC-06 Test coverage:** New entity / storage / migration work includes `#[cfg(test)] mod tests` using `test_db_connection` + `apply_migrations`. New CLI subcommands include parse tests and cover `builtin()` / `builtin_exec()` / `load_mode()`. New integration targets are files under `tests/<name>.rs` (cargo discovers them; no `[[test]]` stanza). Sync this plan's test matrix and `docs/refactoring/integration.md`.
- **GC-07 Secure defaults:** Fail closed unless authn, Cedar authz, path ownership, schema version, object closure, or secret redaction is satisfied. Any fail-open needs an explicit user choice, a log, and a test. If a card's acceptance assumes "authz is on", verify that premise; do not assume a live Cedar `EntityStore`.
- **GC-08 Atomicity and recovery:** Changes to DB transactions, redis, object storage, config, vault secrets, or release state must define transaction boundaries, idempotency keys, the crash window, and rollback / roll-forward.
- **GC-09 Concurrency and resource lifetime:** DB pools, redis connections, file handles, async queues, and temp dirs must have release / recovery semantics. Tests must not depend on unisolated globals (env-mutating tests use `src/config/testing.rs` `env_lock` / `EnvVarGuard`).
- **GC-10 Performance budget:** HTTP hot paths, DB queries, object I/O, Git protocol work, and background jobs must not introduce unbounded scans, unbounded memory, or N+1 DB/network calls. Write data size and assertions when it matters.
- **GC-11 No production panics:** Production paths do not add bare `unwrap()` / `expect()` / `panic!()`. Return `MegaResult`, `anyhow::Context`, or a domain error with an actionable message.
- **GC-12 Precise commits:** Stage only related paths. Do not `commit -a`. If you find unrelated dirty state, leave it and report it. Do not clean, reset, or fold it into the commit. Follow the VCS named in `AGENTS.md` and `.cursor/rules/task-card-release.mdc` (this repo uses **Libra** / `.libra`, not `git`).

## Execution requirements (mandatory)

A task is not complete if any applicable item fails. Cite IDs, not ordinals.

1. **ER-01 Pre-start safety check** — all four, no exceptions:
   - Confirm branch, dirty state, and that target files have no unconfirmed user edits (`libra status` or the equivalent in `AGENTS.md`). If target files already have unconfirmed edits, report and do not overwrite.
   - Object storage is in-tree (`src/orbit_api/` + `src/orbit/`). There is no sibling `../orbit` and no `crates/orbit*` workspace members. A cargo resolve failure is not "missing orbit checkout".
   - Confirm `.env.test` exists (the repo ships `.env.test.example` only; `.env.test` is ignored). If it is missing, stop and ask per `AGENTS.md`. Do **not** silently run `cargo test --all` without sourcing it.
   - Confirm required test services are up: `docker compose -f docker-compose.test.yml up -d --wait` (Postgres `15432`, Redis `16379`, Mailpit `11025/18025`, RustFS `19000/19001`; bucket init needs `--profile init run --rm rustfs-init` or the default `rustfs-init` health wait). Missing Postgres makes related tests panic, not skip.

2. **ER-02 Check, then implement:** Refresh this card's source anchors, doc anchors, test targets, and external revisions. Then decide: implement, add tests, add docs, close, or downgrade.

3. **ER-03 Granularity gate:** Before start, walk `G-*` and the card's `Granularity` summary. If scope has grown (new axis, AC/Verification over limit, scope rose to L, write set overlaps an in-flight card), edit the plan and split first. Do not silently expand during implementation.

4. **ER-04 Per-card acceptance gates:** Gates are **A focused surface gates** (by what actually changed) + **B type gates** (by `Task type`) + **C release close-out** (required for every non-deferred card; who runs them is below) + **D remote post-push gates** (only when CI semantics cannot be reproduced locally). **All applicable rows stack.** All must pass.

   Authoritative submit contract is `AGENTS.md` "Required Checks Before Submitting Code Changes": `cargo +nightly fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `source .env.test && cargo test --all`. This template must not weaken those. `AGENTS.md` also requires `cargo build` and `cargo build --tests` with 0 errors and 0 warnings. Card-specific focused tests are **additional**, not a substitute.

   **Gates do not count toward G-03.** `Verification` lists only this card's own predicates.

   **Two orthogonal status fields:**
   - `Lifecycle`: `pending` | `in-progress` | `blocked` | `done`.
   - `Acceptance`: empty | `locally-accepted` | `remote-pending` | `complete`.
     - `locally-accepted` = applicable A + B passed; C coverage not yet obtained. Do **not** report done.
     - `remote-pending` = A/B passed and C coverage obtained (including one green three-gate run whose tree contains this card's final change), but applicable or inherited D is not green. Do **not** report done.
     - `complete` = A + B passed, C coverage obtained, and applicable or inherited D is green (or D is `N/A`).
     - Only path: A/B pass → `locally-accepted` → ER-05 review PASS → obtain C → (`complete` if no D; else `remote-pending` → D green → `complete`).
   - `Lifecycle=done` requires `Acceptance=complete`. `blocked` must return to `in-progress` before `done`. Plan completion requires every non-deferred card `done`.

   **Who runs C:**
   - Independent release cards and family release-point / `release` cards run the full C set.
   - `family child` cards do not bump, do not build release artifacts, and do not push. They **inherit** C (and D) from the family's unique release point, whose three-gate run must include the child's final tree.
   - `no-release` cards (`docs` / `audit` / `spike` / `handoff`) inherit C and D from the carrier release point named on the card. That ID must not be empty.
   - There is **no** path that replaces the three gates with a zero-hit guard.

   **A — focused surface gates** (one row per production surface you actually changed):

   | Surface changed | Focused gate |
   |---|---|
   | Pure lib unit logic in `monoengine_core` (`#[cfg(test)]` under `src/**`) | `source .env.test && cargo test -p monoengine --lib '<mod::path::tests>'` |
   | Lib logic that needs real Postgres | Start the test stack, then the same `--lib` command; tests must use `test_db_connection` + `apply_migrations` |
   | Process / CLI black-box (`tests/**`) | `source .env.test && cargo test -p monoengine --test <target> -- --test-threads=1 [<filter>]` (`--test <target>` is required) |
   | Bin composition root (`src/main.rs`, `src/bin/migrate_local_to_s3.rs`) | Black-box `--test <target>` via `CARGO_BIN_EXE_monoengine` **plus** `cargo clippy -p monoengine --all-targets -- -D warnings` |
   | CLI parse and registration (`src/cli.rs`, `src/commands/**`) | `cargo test -p monoengine --lib 'cli::tests'` + `cargo test -p monoengine --lib 'commands::'`; new/renamed subcommands assert `builtin()` / `builtin_exec()` / `load_mode()` |
   | HTTP / OpenAPI (`src/api/**`, `src/server/http_server.rs`) | `cargo test -p monoengine --lib 'api::'` + sanitized `/api/openapi.json` from a running server |
   | `src/callisto/**`, `src/jupiter/migration/**` | `migrations()` registration assert + `apply_migrations(&db, true)`; empty `down` ⇒ `Rollback mode = forward-only` |
   | `config/config.toml`, `src/config/**` | `cargo run -p monoengine -- --config config/config.toml config validate`, plus init / `--deny-warnings` / profile / bad-config / no-secret-leak as the change requires |
   | Git protocol / LFS | Local equivalent of `scripts/git_protocol_smoke.sh`; read current `.github/workflows/git-protocol-smoke.yml` for prelude. Default `config.toml` points at 5432/6379, not the test stack — `source .env.test` or export `MEGA_DATABASE__DB_URL` / `MEGA_REDIS__URL` / `MEGA_BASE_DIR`. Push / tag / LFS need a seeded access token in the URL. LFS evidence requires `MONOENGINE_GIT_SMOKE_PUSH=1 MONOENGINE_GIT_SMOKE_LFS=1` and the line `PASS: HTTP LFS push and clone` |
   | Cedar (`src/contract/policy/**`) | Matching tests + an explicit assert of whether this card changed permit-all / empty `EntityStore` |
   | Repo config and CI (`Cargo.toml` non-version lines, `rustfmt.toml`, compose, `scripts/**`, `.github/workflows/**`) | Local equivalent of the affected job, extracted from the workflow file; non-local bits go to D |
   | Docs / index only | No A gate; B structure-and-link gate only |

   This repo is a **single package** `monoengine` (lib `monoengine_core`, bins `monoengine` and `migrate_local_to_s3`). `-p monoengine` is equivalent to omitting it today; still write it so commands stay portable. Focused gates must use `--lib` or `--test <target>`.

   **B — type gates:**

   | Task type | Type gate |
   |---|---|
   | `implementation` / `migration` / `removal` | No extra type gate (A + C complete acceptance; family children run A + fmt/clippy and inherit C) |
   | `docs` / `audit` / `handoff` | Structure and links: product files exist, sections complete, internal links and `file:line` anchors resolve, new `docs/*.md` paths exist, status shows no out-of-scope edits. These types stay no-code / no-config. If code or config is required, reclassify per ER-03 |
   | `spike` | Artifact exists, go/no-go decided, follow-up cards registered; allowlist diff: every change is inside `Deliverables`; zero production-surface edits |
   | `release` | Aggregate guards for new tests this group introduced + release note / compatibility evidence |

   **C — release close-out** (order is mandatory), run by cards that push: ① ER-08 version-face parity precheck → ② bump `Cargo.toml` `version` per `Version increment` and let the toolchain refresh `Cargo.lock` → ③ three `AGENTS.md` gates on the **bumped** tree → ④ `cargo build` and `cargo build --tests` clean; add `cargo build --release -p monoengine` if you need a binary → ⑤ commit (ER-07) → ⑥ push and confirm the remote ref moved → ⑦ tags and release artifacts default to `N/A` (this repo has no tag-triggered release pipeline).

   **C / D boundary:** C ends at a verified branch push. Everything a remote pipeline produces after that is D. Each D item records workflow file, job name, trigger event and ref, whether `paths:` matches this card, and the predicate.

   **D — remote post-push:** only when CI semantics cannot be reproduced locally (for example `paths:`-filtered jobs on `push` to `main`, secret-existence checks). If this card's paths hit no workflow filter, D is `N/A`. D does not block `locally-accepted` or ER-05. D failure is roll-forward only (new commit / new version), never revert of an already-pushed commit.

5. **ER-05 Review loop:** After implementation and local acceptance, review. Fix findings and re-run acceptance until review says `PASS`. P0/P1 must close. "Residual risk accepted" cannot replace `PASS` except for P2 with a named owner in writing.

6. **ER-06 Docs and compatibility:** Public-behavior cards sync user docs, developer docs, `config/config.toml` samples, the error contract, runtime OpenAPI evidence, and the test matrix.

7. **ER-07 Commit workflow:** Follow `AGENTS.md` and the task-card release rule. Stage related paths only, commit with a scoped message, push the agreed branch. Do not `--force`. If local and remote have diverged, stop and report. After commit, verify the signature / sign-off convention this repo actually uses (do not invent a second convention in the card).

8. **ER-08 Version and release:** The version source of truth is root `Cargo.toml` `version`. After the single-package inline there is **one** version face. On start day, re-count version-face files (`rg -n '^version' Cargo.toml` and `rg -l '^\[package\]' --glob '**/Cargo.toml'`). `Version increment`: `patch` (default) | `minor` | `major` | `N/A`. Breaking public-surface or schema/protocol changes must be `minor` or `major`. Family children and `docs` / `audit` / `spike` / `handoff` cards are `N/A` and must say which commit lands their artifacts. Cargo commands use `-p monoengine`.

9. **ER-09 Push failure:** Non-fast-forward: pull/merge, re-accept, then push. Auth, permission, network, or server failures are not blind-retried; record the reason and wait for the next fix/release window.

10. **ER-10 Internal service errors (bounded retry):** Redis, Postgres, object storage, SMTP, and AI-provider errors do not mark a task done. Deterministic 4xx / schema / compile / config defects are not retried. Fix in-card only if the fix stays on this card's axis **and** re-running ER-03 still passes; otherwise open `FIX-*`, add `FIX-* -> current card`, and set the current card `blocked`. Transient errors retry with exponential backoff (default ≤ 5 attempts, ≤ 30 minutes total). After budget, set `blocked` with sanitized evidence. Release pushes do not auto-retry (ER-09).

11. **ER-11 Evidence hygiene:** Acceptance evidence must not store secrets, API keys, tokens, PII, unsanitized transcripts, private absolute paths, or raw tool payloads. Public test passwords (`monoengine_test_password`, `smtp-test-password`, RustFS `rustfs` / `rustfs_secret`) are still redacted in records.

12. **ER-12 Concurrency vs serial release:** Concurrent work is allowed only in implementation and review, and only when `Implementation write set`s are disjoint (G-10). **Release is always serial and has one publisher:** bump, build, commit, push, and D tracking. Only one card may be in "bumped but not yet pushed" at a time. Do not invent an unverified document lease as a repo lock.

## Implementation order

Edge format: `A -> B` means A before B. The graph must be acyclic. Every edge points at a card, not a whole Phase (G-06). Update this section when cards split.

- `<TASK-01> -> <TASK-02>`
- `<TASK-02> -> <TASK-03>`

### Dependency register

Every dependency outside this plan (other dated plans, external services, human approval, upstream revision) and every scope this plan hands off must be registered here before a card may cite it (G-06). In-plan edges use task IDs only.

`direction`: `incoming` = this plan waits; `outgoing` = this plan hands scope out (Owner is the receiver; timeout policy is what happens if they do not take it).

| ID | direction | Type | Object | Owner | Artifact and ready predicate | Evidence | Timeout / failure |
|---|---|---|---|---|---|---|---|
| DEP-01 | `<incoming / outgoing>` | `<cross-plan / external service / approval / upstream revision>` | `<plan-YYYYMMDD#TASK-ID, plan-long#PT-NN, or external>` | `<person / system / receiving plan>` | `<what is delivered and how we know it is usable>` | `<file:line / commit / URL + check date>` | `<wait cap, degrade or fallback>` |

### Release groups and concurrency windows

Default: each card is its own release (G-07). Fill this table only for merged releases or explicit concurrency/serial windows. Registered items must be cited from the card `Release boundary`.

| ID | Members | Unique release point | Window rule | Failure rollback order | Why |
|---|---|---|---|---|---|
| REL-01 | `<TASK-ID list>` | `<TASK-ID>` | `<e.g. no-push window: children commit locally only>` | `<reverse-deps revert local commits and re-run ER-04>` | `<why this cannot be independent slices>` |

**Concurrency statement:** `<groups whose I sets are disjoint / fully serial>` (G-10)

**Publisher:** `<the one Agent or person who runs C and tracks D>` (ER-12)

**Release window order:** `<order of REL-* / independent cards; only one card bumped-not-pushed at a time>`

### Phase 0: <baseline freeze>

**Goal:** `<goal>`

**Entry:**

- `<precondition>`

**Exit:**

- `<done predicate>`

### Phase 1: <first releasable slice>

**Goal:** `<goal>`

**Entry:**

- `<precondition>`

**Exit:**

- `<done predicate>`

## Task cards

Use a stable prefix (`IT-01`, `A0-01`, `DR-01`). Once cited, numbers are not reused. Split children append new numbers at the end of their Phase. **Number order is not execution order.** Execution follows the dependency edges. Retired numbers stay, with a replacement note.

### Task-card granularity (mandatory)

Granularity is the first quality bar. A card that is too large cannot be reviewed, rolled back, or finished by one Agent. A card that is too small splits implementation from tests and docs and pays a release cost every time. New or edited cards must satisfy every `G-*`. Failure means "not ready to start": split or merge, then sync order, DEP, REL, traceability, test matrix, milestones, risks, and revision history.

- **G-01 One recoverable axis:** One card owns one independently recoverable behavior axis. On failure or withdraw, one declared recovery action leaves the system in a coherent state. Recovery is not "one revert": irreversible work is fine if the path is single and written down. Do not pack "schema + storage + API", "delete A + B + C", or "new capability + drive-by refactor" into one card. If `Description` has two coordinating "and / also / while we're here" goals, split. `Rollback mode` is one of:
  - `revert` — local code/docs; one revert undoes it (default).
  - `forward-only` — irreversible data or migration (Postgres, object store, vault). Write invariants, recovery commands, and user impact. Many existing `down` methods are no-ops; the runner exposes `up` and destructive `refresh` only. Treat existing migrations as `forward-only` unless this card implements and proves a real `down`.
  - `compensating` — external side effects. Write the compensating command and idempotency key.
  - `immutable-release` — already pushed; only a new version. Write downgrade guidance and the compatibility window.
- **G-02 Complete deliverable:** Implementation, tests, docs, and index sync for the same axis are **one card**. Split only when the extracted part is itself a recoverable axis (independent migration, deprecation close-out, performance gate, API facet, cross-plan handoff).
- **G-03 Item caps** (independent predicates, not line count; ER-04 gates do not count):

  | Task type | AC cap | Verification cap |
  |---|---|---|
  | `implementation` / `migration` / `removal` | 8 | 8 |
  | `spike` | 8 | 8 |
  | `docs` / `audit` / `handoff` | 20 | 20 |
  | `release` | 12 | 12 (aggregate guards, release note, compatibility evidence) |

  AC counts independent pass/fail predicates (`and` / nested bullets / table rows). Verification counts independent gates (env prelude + following command = one gate; multiple `--test` targets = multiple; `&&` of two judging commands = two). Over limit means multi-axis: split. Do not hide extras in long sentences or "etc."

- **G-04 Size cap:** Start-state `Estimated scope` is `S` or `M` only. `L`/`XL` after the plan is written is a defect. Count **behavior landings and production files** only. Do not count this card's own tests, GC-05/ER-06 docs, or the ER-08 version face (still list them in the write sets).
  - `S`: ≤ 2 behavior landings, ≤ 3 production files, no schema / protocol / public-interface change.
  - `M`: ≤ 4 landings, ≤ 12 production files, at most one public behavior or interface change, still one axis.
  - Over `M` is L: split by default. Mechanical repo-wide rename/delete/format may take `L-exception:EX-<n>` with a named waiver. `XL` is never a start state. Counting `src/` or repo root as "one landing" is inflation.
- **G-05 Agent-executable alone:** `Current evidence` has checkable `file:line` anchors. `Acceptance criteria` are self-contained. `Verification` is copy-paste commands. `Dependencies` cite `DEP-*` or task IDs only. No "see above" or "same as the last card". Shared conventions become a GC or an ADR.
- **G-06 Closed acyclic deps:** In-plan edges use task IDs. Cross-plan and external prelude must be `DEP-*` first. "Wait for Phase N" is a split error. Implementation-order edges and each card's `Dependencies` must match; if they differ, the order section wins and the card is fixed on the spot.
- **G-07 Release-slice alignment:** Default one card = one release slice. `implementation` / `migration` / `removal` take a full slice. `docs` / `audit` / `spike` / `handoff` are `no-release` and say which commit lands them. `release` cards are the release point. Merged releases are exceptions and must be `REL-*` before start.
- **G-08 Family cards:** When a public-surface delete or a schema+reader pair cannot ship as independent slices, split into children that each review and each pass applicable ER-04 gates and each commit locally, sharing one `release` card as the unique release point. Children: `family child`. Point: `family release point`. Register the no-push window as `REL-*`.
- **G-09 Split protocol:** The original number stays on the main axis. New children append new numbers. Old card says "split out `<ID>`". New card says "split from `<ID>`". Sync order, DEP, REL, traceability, test matrix, milestones, risks, and one revision-history row.
- **G-10 Write sets and concurrency:**
  - **I** — code, tests, docs that carry this card's behavior.
  - **R** — `Cargo.toml` `version` + `Cargo.lock` (or `N/A` for family child / no-release).
  - **C** — plan-level release order (ER-12). Not a card field.
  - I–I overlap → no concurrent work (no waiver): add an order edge or merge into one integration card.
  - I–R overlap → the open release window write-locks R files.
  - R–R overlap → serialized by ER-12, not a concurrency ban.
  - Concurrency is judged on `Implementation write set`, not `Files likely touched`.
- **G-11 Task type:** `implementation` (default) | `migration` (usually `forward-only`) | `removal` (usually a family) | `spike` (no production code; time-box S ≤ 0.5 person-day, M ≤ 2) | `audit` / `docs` (product-file or person-day caps) | `release` | `handoff` (`no-release`; incoming/outgoing in the DEP table).

#### Recommended split dimensions

| Dimension | Cut | Typical result |
|---|---|---|
| Data / state | entity + migration → storage write / idempotency → read projection / recovery | 3 cards |
| Protocol | version / negotiate → capacity / backpressure → consumer attach | 3 cards |
| Surface | service/storage → machine interface → CLI or client | 2–3 cards |
| Lifecycle | new impl → default flip → deprecation shim → physical delete | cards per release window |
| Security | identity / request boundary → path / object ownership → redaction | one card per axis |
| Cleanup | public-surface delete (family) → internal module exit → dependency drop | family + ordinary cards |

#### Granularity anti-patterns

| Anti-pattern | Symptom | Fix |
|---|---|---|
| Giant card | scope = L; AC > 8; several goals in Description | Split on the table above |
| Fragment card | "add tests" / "add docs" / "rename a field" alone | Merge back onto the axis (G-02) |
| Multi-axis disguise | long sentence or "etc." to stay under 8 | Recount predicates (G-03) |
| Landing inflation | `src/` or repo root counted as one landing | Recount at directory grain (G-04) |
| Implicit dependency | "per card X's convention" that X never delivered | Write it here or promote to GC / ADR (G-05) |
| Ghost acceptance | only `cargo test --all`, or `rg` that cannot tell miss from error | Name package/target/fn; use the exit-code template |
| Dangling dependency | "Phase N done" or free-text external prelude | Converge to task IDs / `DEP-*` (G-06) |
| Fake rollback | already pushed or migrated, still "one revert" | Choose `forward-only` / `compensating` / `immutable-release` |
| Concurrent collision | two independent cards share I | Order edge or merge (G-10, no waiver) |
| Drive-by merge | several cards released as one from a scratch note | Register `REL-*` or split back (G-07/G-08) |

#### Field defaults and exceptions

After this section declares defaults, cards may omit fields that take the default, or write `Inherited`. Only deviations are expanded and listed below. Required on every card: `Task type`, `Lifecycle / Acceptance`, `Rollback mode`, `Implementation write set`, `Version increment`, `C/D coverage from`, `Granularity`. `Release write set` may be `Inherited` or `N/A`. `Deliverables` is required on `docs` / `audit` / `spike` / `handoff`.

- **Release boundary default:** `<independent / other>`
- **Task type default:** `<implementation / other>`
- **Rollback mode default:** `<revert / other>`
- **Migration and rollback default:** `<N/A: no schema migration / other>`
- **Security and privacy default:** `<inherit GC-07, GC-11 / other>`
- **Performance budget default:** `<inherit GC-10 / other>`
- **Docs and compatibility default:** `<sync related docs/ and config samples per GC-05 / other>`

**Default overrides** (not waivers):

| Task | Field | Value and why |
|---|---|---|
| `<ID>` | `<Rollback mode>` | `<forward-only: existing down is empty>` |
| `<ID>` | `<Docs and compatibility impact>` | `<dev docs only>` |

**Rule waivers (`EX-*`, named approval).** Only these three rules may be waived. `G-01`, `G-02`, `G-05`–`G-11` are **never** waivable.

| Waivable | Allowed reasons |
|---|---|
| G-03 item cap | Checklist products (docs / audit / index) truly need more items, with a file list |
| G-04 size cap (`L-exception`) | Mechanical: repo-wide rename, bulk delete, format |
| ER-07 signing | Named repo-policy exception (sign-off-only) |

| Exception ID | Task (or `ALL/<scope>`) | Waived rule | Reason and compensation | Approver | Review round | Evidence | Expires |
|---|---|---|---|---|---|---|---|
| EX-01 | `<ID>` | `<G-03>` | `<docs-only; product files = …>` | `<name>` | `<R-n>` | `<file:line / review>` | `<this plan / YYYY-MM-DD>` |

#### Granularity audit table

Fill after the plan is written and after every normative edit. Any failing column blocks start. Over-limit `AC` / `VER` / `scope` must carry `@EX-ID` or `L-exception:EX-n` that exists in the waiver table, names this card (or `ALL/<scope>`), matches the exceeded rule, uses an allowlisted reason, and is still in date.

| Task | type | axis | recovery | complete | self-contained | AC | VER | landing / prod-files | scope | deps | writeset | release | split-from | exception |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `<ID>` | `<Task type>` | `<axis>` | `<recovery>` | `<yes>` | `<yes>` | `<n/cap[@EX-ID]>` | `<n/cap[@EX-ID]>` | `<n>/<n>` | `<S/M/L-exception:EX-n>` | `<TASK-ID/DEP-ID/none>` | `<no-overlap/serialized-on ID>` | `<independent/REL-n child/REL-n point/no-release>` | `<ID/N/A>` | `<EX-ID/N/A>` |

#### Verification judgment

Zero-hit guards must distinguish "no match" from "command failed". `rg` exits `0` = hit, `1` = zero hits, `>1` = error. Do not use `! rg` or `if rg; then exit 1; fi` alone. Use:

```bash
if rg -n "<pattern>" <paths>; then
  echo "FAIL: forbidden pattern found"; exit 1
else
  rc=$?
  if [ "$rc" -ne 1 ]; then echo "ERROR: rg failed with exit $rc"; exit "$rc"; fi
  echo "OK: zero hits"
fi
```

- Allowlist guards compare each hit to a fixed list and attach the hit diff.
- `rg` used only to find a symbol must say "anchor location, not a predicate".
- New test fns / filters are marked `(new)` with a clear home: `--lib '<mod::path::tests::fn>'` or `--test <target>`.
- `cargo test --all` does not replace the card's focused tests (ER-04). Focused tests do not replace the plan-completion three gates.
- Tests that need real services must state the prelude. Missing Postgres **panics**. Treating skip as pass is ghost acceptance.

### Task <ID>: <title>

**Task type:** `<implementation | migration | removal | spike | audit | docs | release | handoff>` (G-11)

**Lifecycle / Acceptance:** `<pending | in-progress | blocked | done>` / `<empty | locally-accepted | remote-pending | complete>` (ER-04; `done` requires `complete`)

**Description:** `<what, why, user impact. One sentence names the single axis.>`

**Out of scope:** `<each item: "owned by <ID>" / "not scheduled; restart when …" / "permanent non-goal because …">`

**Current evidence:**

| Fact | Evidence |
|---|---|
| `<current impl or gap>` | `<file:line / test / external repo@sha>` |

**Acceptance criteria:**

- [ ] `<user-visible or system predicate>`
- [ ] `<API / config / schema / error predicate>`
- [ ] `<failure-path / edge predicate>`
- [ ] `<docs / compatibility / migration predicate>`

**Verification:**

- [ ] `<exact command>`
- [ ] `<exact command>`
- [ ] `<manual / sanitized evidence, if required>`

**Dependencies:** `<none / this-plan Task IDs + the concrete artifact consumed / DEP-ID>` (G-06)

**Deliverables:** `<required for docs / audit / spike / handoff: product file list. Code cards: N/A or Inherited.>`

**Implementation write set:** `<files or dirs that carry this card. Concurrency is judged only here.>` (G-10)

**Release write set:** `<Inherited (= Cargo.toml version + Cargo.lock) / N/A>` 

**Files likely touched:** `<src/...>, <tests/...>, <config/...>, <docs/...>` (estimate)

**Docs and compatibility impact:** `<Inherited / concrete files>`

**Rollback mode:** `<revert | forward-only | compensating | immutable-release>` (G-01)

**Migration and rollback:** `<N/A or up/down, roll-forward, invariants, recovery commands, user impact; say so if down is empty>`

**Security and privacy:** `<N/A or Cedar, secrets, paths, redaction, input checks>`

**Performance budget:** `<N/A or data size, complexity, wall-clock / benchmark>`

**Estimated scope:** `<S / M / L-exception:EX-<n>>` (G-04)

**Version increment:** `<patch | minor | major | N/A>` (ER-08)

**Release boundary:** `<independent | family child of REL-<n> (k/n) | family release point of REL-<n> | no-release>` (G-07)

**C/D coverage from:** `<self | <TASK-ID>>` (ER-04; family child and no-release must name an ID)

**Granularity:** `type=<Task type>; axis=<single axis>; recovery=<one recovery action and coherent post-state>; complete=<yes>; self-contained=<yes>; AC=<n>/<cap>[@EX-ID]; VER=<n>/<cap>[@EX-ID]; landing=<n>; prod-files=<n>; scope=<S|M|L-exception:EX-n>; deps=<none|TASK-ID,…|DEP-ID,…>; writeset=<no-overlap|serialized-on TASK-ID>; release=<independent|REL-n child|REL-n point|no-release>; split-from=<TASK-ID|N/A>; exception=<EX-ID[,…]|N/A>`

Mapping: `type`→G-11, `axis`/`recovery`→G-01, `complete`→G-02, `AC`/`VER`→G-03 (caps 8 / 12 / 20 by type; ER-04 does not count), `landing`/`prod-files`/`scope`→G-04, `self-contained`→G-05, `deps`→G-06, `release`→G-07/G-08, `split-from`→G-09, `writeset`→G-10, `exception`→registered `EX-*`. If you cannot fill this line, the card is not split cleanly.

## Test matrix

| Class | Must cover | Target / command |
|---|---|---|
| Unit | `<pure logic, config parser, error map>` | `<cargo test -p monoengine --lib '<mod::tests>'>` |
| Integration (DB) | `<real Postgres + storage/migration>` | `<--lib with test_db_connection + apply_migrations>` |
| Integration (process) | `<real binary, service start, config parse>` | `<cargo test -p monoengine --test <target> -- --test-threads=1>` |
| CLI | `<parse, three registrations, exit codes, output>` | `<cargo test -p monoengine --lib 'cli::tests'>` |
| HTTP API | `<routes, status, JSON schema, auth>` | `<cargo test -p monoengine --lib 'api::...'>` |
| Migration | `<up/down, old/new schema, data move>` | `<migration tests>` |
| Config | `<validate / init / profile / secret leak>` | `<cargo run -p monoengine -- ... config validate ...>` |
| Git protocol | `<clone/fetch/push/shallow/v2/LFS>` | `<scripts/git_protocol_smoke.sh local equivalent>` |
| Security | `<authn, Cedar, secrets, path traversal, redaction>` | `<cargo test ...>` |
| Performance | `<size and budget>` | `<criterion / wall-clock>` |
| live/gated | `<RustFS / Mailpit / SMTP>` | `<compose profile or env-gated command>` |

## Traceability

| Task | Source / evidence | mega2 landing | Docs / compatibility | Named tests |
|---|---|---|---|---|
| `<ID>` | `<file:line / issue / repo@sha>` | `<src/callisto / src/jupiter / src/api / …>` | `<docs/..., config/config.toml, runtime OpenAPI>` | `<-p monoengine --lib or --test>` |

## Milestone acceptance and rollback

| Milestone | Done when | Release / evidence | Rollback or roll-forward |
|---|---|---|---|
| M0 | `<baseline frozen>` | `<commit/test/doc>` | `<N/A>` |
| M1 | `<first releasable slice>` | `<version/test/review>` | `<rollback/forward fix>` |

### Failure-recovery matrix

| Failure point | Acceptable residue | Recovery | Forbidden outcome |
|---|---|---|---|
| `<mid-transaction, pre-commit>` | `<temp table / partial write>` | `<retry/abandon/rollback>` | `<data loss / partial commit / silent success>` |

## Risk register

| Risk | Impact | Mitigation | Task |
|---|---|---|---|
| `<risk>` | `<high/med/low + impact>` | `<test/design/gate>` | `<ID>` |

## Performance and capacity summary

| Operation | Per-call cost | Cumulative cost | Budget / cap | Proof |
|---|---|---|---|---|
| `<op>` | `<O(...)>` | `<O(...)>` | `<threshold>` | `<test/benchmark>` |

## Compatibility and docs close-out

- [ ] `docs/errors.md` synced, or `N/A`.
- [ ] Related `docs/refactoring/*.md` synced, or `N/A`.
- [ ] `config/config.toml` samples and comments synced, or `N/A`.
- [ ] Runtime OpenAPI (`/api/openapi.json`) evidence captured, or `N/A` (no on-disk spec).
- [ ] `README.md` / `AGENTS.md` constraints synced or drift registered, or `N/A`.
- [ ] `src/callisto/`, `src/jupiter/storage/`, `src/jupiter/migration/` (including `migrations()`) synced, or `N/A`.
- [ ] New `docs/*.md` citations all exist, or `N/A`.
- [ ] `plan-long.md` dated-plan index or PT/SB status synced, or `N/A`.

## Review log

Result is only `PASS` or `FAIL`. `FAIL` must list P0/P1 and close them next round. P2 may be accepted in writing by a named owner; that does not change this round's `FAIL` (ER-05).

| Round | Scope | Result | P0/P1 | P2 disposition | Evidence |
|---|---|---|---|---|---|
| R1 | `<files/tasks>` | `<PASS / FAIL>` | `<items and close state>` | `<fix / named accepter>` | `<commands / re-review>` |

## Non-goals and deferred items

| ID | Deferred | Why | Restart when | Owner |
|---|---|---|---|---|
| DEFER-<PREFIX>-01 | `<what>` | `<why>` | `<when>` | `<plan/PT/ADR>` |

## Completion criteria

The plan is complete only when all of the following hold:

- [ ] Every card satisfies `G-*`: no unregistered L exception, no XL card, no fragment cards, no unregistered merged release, I-set conflicts resolved; granularity audit table filled.
- [ ] Every non-deferred card has met acceptance **and** `Lifecycle=done` **and** `Acceptance=complete` (ER-04). `remote-pending` cards must first get green D. `blocked` cards must unblock or become `DEFER-*`.
- [ ] Every card's Verification commands have been run and recorded.
- [ ] **Plan completion gates:** `cargo +nightly fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `source .env.test && cargo test --all` all green; `cargo build` and `cargo build --tests` 0 errors / 0 warnings; no new crate-level `#[allow(...)]`.
- [ ] Required docs / config / error contract / test-matrix updates are done.
- [ ] Required migration, rollback, and failure-recovery checks are done; each card's `Rollback mode` was actually proven or recorded as unprovable.
- [ ] Final review is `PASS`; all P0/P1 closed; only named P2 residual risk remains.
- [ ] If release is required, the version face is bumped and consistent, and build / commit / push evidence exists (ER-08); otherwise `N/A` with reason.
- [ ] Revision history records every post-draft normative change (G-09).
- [ ] Related `plan-long.md` PT/SB status or dated-plan index is synced, or `N/A`.
