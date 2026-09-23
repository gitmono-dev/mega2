# Configuration Reference

English · [中文](configuration.zh.md)

This document is the operator's guide to the mega2 configuration system: load order, secret management, hot reload, a per-section tour, startup validation, and `config validate`. The fully commented per-key sample is authoritative in [`config/config.toml`](../config/config.toml) and is not duplicated here; per-subsystem contract details live in [`refactoring/config.md`](./refactoring/config.md) (frozen semantics for object_format / cedar / push_auth / storage_events, etc.). Product rules: [`monorepo.md`](./monorepo.md). Trunk deployment operations: [`deploy-trunk.md`](./deploy-trunk.md).

The config module lives in `src/config/` (loader / model / source / expand / secret / validate / reload). Global flags are `--config <path>` and `--profile <name>` (equivalent environment variables `MEGA_CONFIG` / `MEGA_PROFILE`).

## 1. Load order and sources

The base config file is the **first hit** in this order (`src/config/loader.rs`):

1. `--config <path>` (CLI; source name `cli`)
2. `MEGA_CONFIG` environment variable (`env`)
3. `./config/config.toml` (current working directory; only if it exists, `cwd`)
4. `$MEGA_BASE_DIR/etc/config.toml` (`global`)
5. None of the above → a default config is generated and written to `$MEGA_BASE_DIR/etc/config.toml` (`default_generated`)

Read-only / existing-config commands (e.g. `config validate`, `service init`) use `load_readonly` / `load_existing`: a file named via rules 1–2 that does not exist is an immediate error, and they **never** generate a default config — no side effects on the very system they were asked to observe.

**Profiles**: `--profile prod` (or `MEGA_PROFILE=prod`) selects the sibling file `config.prod.toml` next to the base config (derived from the base file name: `<stem>.<profile>.<ext>`). Profile names allow only ASCII letters / digits / `-` / `_`; a missing profile file is an error. CLI `--profile` wins over `MEGA_PROFILE`. Merge order: **base file → profile file → environment variables** (the profile is merged before env; env always wins last).

**Environment overrides**: the pattern is `MEGA_<SECTION>__<KEY>`; the double underscore is the hierarchy separator (`src/config/source.rs`). Example:

```bash
MEGA_DATABASE__DB_URL='postgres://mono-pg:5432/mono' \
MEGA_OAUTH__ALLOWED_CORS_ORIGINS='https://app.example.com,https://app2.example.com' \
  mega2 --config /etc/mega2/config.toml service http
```

**Strict mode**: unknown and removed fields are rejected at parse time (`reject_unknown_fields`), never silently ignored — a leftover `[mail]` section or `[monorepo].merge_writer` key fails startup. `config validate` reports ignored compatibility fields as warnings (without printing secret values); `--deny-warnings` escalates them to failure in automation.

## 2. Secret management

Credentials are never committed in `config.toml`. Two injection mechanisms:

- **SecretRef**: `vault://secret/<name>#<field>`, resolved through the embedded Vault (`libvault` crate + `src/contract/vault/`; contract in [`refactoring/vault.md`](./refactoring/vault.md)). `SecretRef`'s `Debug` / `Display` are always the redacted `vault://secret/***#***`; error text contains neither the real path nor the value.
- **File mounts**: `${file:/run/secrets/xxx}` expands to the file contents at load time (`src/config/expand.rs`). A value containing `${file:...}` must consist only of placeholders and literal text — it cannot be mixed with `${var}`; an unterminated `${file:` is rejected as well.

Config secret fields that may be stored in the mega2 Vault (the only ones `config secret` accepts; see `src/commands/config.rs`): `redis.url`, `notification.webhook.token`, `object_storage.s3.access_key_id`, `object_storage.s3.secret_access_key`, `storage_events.targets.<id>.secret_ref`. **Database credentials are deliberately excluded** — they are the Vault's own bootstrap dependency and must stay in deployment / environment secrets. Each field has a fixed namespace `config/<profile>/<suffix>` (e.g. `config/prod/redis/url`); `config validate` checks the shape only, never resolves.

```bash
# Print the canonical URI (does not touch the Vault)
mega2 --config /etc/mega2/config.toml config secret ref \
  --vault-path config/prod/redis/url --field value

# Store / rotate (the value is read from stdin only; --value-stdin is required)
printf '%s' "$REDIS_URL" | mega2 --config /etc/mega2/config.toml \
  config secret set redis.url \
  --vault-path config/prod/redis/url --field value --value-stdin
mega2 --config /etc/mega2/config.toml config secret rotate redis.url \
  --vault-path config/prod/redis/url --field value --value-stdin < /run/secrets/new-redis-url

# Verify resolvability (does not print the value)
mega2 --config /etc/mega2/config.toml config secret check redis.url \
  --vault-path config/prod/redis/url --field value
```

After `rotate`, restart services that consume the secret; new resolutions and `config validate --resolve-secrets` use the rotated value immediately.

**Vault operations** (`config vault *`, all load via VaultBootstrap and never start a service): `reset` (rebuild; destructive, requires `--force`; the previous core key is backed up automatically), `rekey` (re-splits the unseal shares, requires `--force`; note it only re-splits the current KEK — previously exported share sets still unseal the vault, and invalidating them requires a full KEK rotation), `backup <destination>` (exports the core key plus `.meta.json`), `restore <source>` (overwrites core_key.json, requires `--force`, verifies the vault unlocks after restore). `--key-path` defaults to `core_key.json` in the standard vault data directory.

**[vault.audit]**: secret-access auditing, enabled by default. `sink = "tracing"` (default; the `vault_audit` target, infallible) or `"file"` (append-only JSONL, fsync'd per record, requires `file_path`); with `fail_closed = true`, a failed audit write fails the secret operation itself (default false, fail-open). Audit records contain only the operation, logical name, outcome, and caller — **never the secret value**.

## 3. Hot reload

While a service runs, `ConfigReloadWatcher` polls the base config and profile file mtimes/sizes every **5 seconds** (`CONFIG_RELOAD_POLL_INTERVAL` in `src/commands/service/mod.rs`). A candidate config first passes full `Config::validate`; an invalid one is rejected and the current snapshot is kept. A valid candidate is split per field (`src/config/reload.rs`):

**Applied live** (`applied_fields`; if a subscriber's apply fails, everything rolls back and the new snapshot is not published):

- `log.level` / `log.print_std` / `log.with_ansi`
- `artifacts_gc.interval_secs` / `grace_secs` / `batch_limit`; `artifacts_gc.enable` applies live only **true → false** — enabling from false requires a restart
- `buck.cleanup_interval` / `completed_retention_days`; `buck.enable_session_cleanup` applies live only **true → false** — enabling from false requires a restart
- `notification.enabled` (the notification section snapshot is swapped live; an enabled flip counts as applied)

**All other fields** (`database.*`, `redis.url`, `base_dir`, `monorepo.*`, `git.*`, `pack.*`, `lfs.*`, `blame.*`, `object_storage.*`, `oauth.*`, `storage_events.*`, `github_sync.*`, buck upload limits, `vault.audit`, `cedar`, etc.) are only recorded in `restart_required_fields` — visible in the logs, snapshot unchanged — and take effect on process restart. Hot reload never writes new secret values back into the snapshot, and reports never contain secret values.

## 4. Section guide

One line of purpose and the key entries per section; full comments and defaults are authoritative in [`config/config.toml`](../config/config.toml). Everything is restart-required except the whitelist in §3.

- **`base_dir`** (top level): data root (logs, local objects, LFS, caches), overridable via `MEGA_BASE_DIR`; the `${base_dir}` placeholder expands inside path fields in the sample.
- **`[log]`**: tracing logs. `level` (trace..error), `print_std` (off in production), `with_ansi` (stdout only). All hot-reloadable.
- **`[database]`**: PostgreSQL only (`db_type = "postgres"`). `db_url`, pool `max_connection` / `min_connection`, `acquire_timeout` / `connect_timeout`, `sqlx_logging`. Inject credentials via `MEGA_DATABASE__DB_URL`, not via Vault SecretRef.
- **`[monorepo]`**: product rules (authoritative: [`monorepo.md`](./monorepo.md)). `import_dir` (default `/third-party`, the ImportRepo multi-branch exception), `admin`, `root_dirs` (directory init), `object_format` (`sha1` default; `sha256` / `blake3` are Libra extensions — see [`refactoring/config.md`](./refactoring/config.md) and [`refactoring/protocol.md`](./refactoring/protocol.md)), `push_policy` (`review` default / `trunk`), `max_push_commits` (trunk chain-length bound). Bootstrap: `mega2 --config <path> service init --yes` (see [`manual/monorepo-init.md`](./manual/monorepo-init.md)).
  - Path shape (`config validate`, service startup and hot-reload candidates run the same check; a violation fails and names the field): every `root_dirs` entry is a unique single path component — no `/`, `\` or NUL; not `.`, `..` or empty; no leading/trailing spaces; not a reserved root entry (`.cedar`, `.mega_cedar.json`, `.buckroot`, `.buckconfig`, `.git`); `import_dir` is a canonical absolute non-root path (no trailing slash, no `//`, `.` or `..` segments, no NUL or `\`) whose first component is listed in `root_dirs`. **Import first**: a path under `import_dir` is always an ImportRepo (created by Git push), even if `root_dirs` has the same name. `root_dirs` only creates directories at the first initialization (`service init`, or the first service start on an empty database); changing it later needs a restart and does not add or remove top-level directories of an initialized repository (see [`manual/monorepo-init.md`](./manual/monorepo-init.md)).
- **`[monorepo.rename]`**: move/rename detection for diff classification: `similarity_threshold` (0-100), `rename_limit` (0 = unlimited).
- **`[pack]`**: receive-pack decode resources. `pack_decode_mem_size` / `pack_decode_disk_size` (K/M/G, KiB/MiB, and percentages supported), `pack_decode_cache_path`, `clean_cache_after_decode`, `channel_message_size`, `save_entry_concurrency`.
- **`[lfs]`**: `[lfs.ssh].http_url` (href base for the SSH transfer; LFS payloads still move over HTTP), `[lfs.local].lfs_file_path`. In trunk, LFS authorization follows `git.push_auth` — see [`deploy-trunk.md`](./deploy-trunk.md) §6.
- **`[object_storage]`**: global backend for Git blobs / LFS / artifacts, `storage_type = "local" | "s3" | "s3compatible" | "gcs"`, with sub-sections `[object_storage.s3]` (region / bucket / access_key_id / secret_access_key / endpoint_url), `[object_storage.gcs]`, `[object_storage.local]`. The backend is built via `build_object_storage`; contract in [`refactoring/orbit.md`](./refactoring/orbit.md). `config validate` rejects `vault://` values in access_key_id / secret_access_key — inject bootstrap credentials via env / profile / deployment secrets.
- **`[oauth]`**: website Better Auth session validation. `website_api_base_url`, `session_cookie_names` (omitted = Better Auth default cookie names), `allowed_cors_origins` (empty = built-in development defaults; env override is a comma-separated list). Trust boundary: [`refactoring/website-auth.md`](./refactoring/website-auth.md).
- **`[blame]`**: large-file thresholds and traversal resources: `max_lines_threshold`, `max_size_threshold`, `default_chunk_size`, `max_commits_in_memory`, `enable_caching`.
- **`[redis]`**: `url`. Cache / distributed locks / snowflake worker leases, etc.; may be Vault-backed via SecretRef.
- **`[buck]`**: Buck upload API: session and file limits (`session_timeout` / `max_file_size` / `max_files` / `max_concurrent_uploads`), server-side rate limiting (`upload_concurrency_limit` / `large_file_concurrency_limit` / `large_file_threshold`), session cleanup task (`enable_session_cleanup` / `cleanup_interval` / `completed_retention_days`, partially hot-reloadable — see §3).
- **`[artifacts_gc]`**: GC of unreferenced artifact blobs (`enable` / `interval_secs` / `grace_secs` / `batch_limit`), default off; tuning hot-reloads, enabling from false requires a restart.
- **`[notification]`**: `enabled` global kill switch (hot-reloadable); optional `[notification.webhook]` outbound channel (`url` is non-secret, `token_ref` is a SecretRef). Behavior and boundaries: [`refactoring/notification.md`](./refactoring/notification.md).
- **`[vault.audit]`**: see §2.
- **`[cedar]`**: `enforcement = "off" | "shadow" | "enforce"` (ADR-UN-01; default off). Trunk morphology requires `off`. Semantics and snapshot building: [`manual/authz.md`](./manual/authz.md).
- **`[git]`**: authentication for the Git protocol and product API writes. `push_auth` (omitted = OAuth/UserStorage chain, review only; `"token"` / `"none"` require trunk), `[[git.push_tokens]]` (name / token / paths, component-boundary prefix authorization; tokens via `${file:...}` or SecretRef), `ssh_receive_pack` (storage-only must set it to `false` explicitly), `anonymous_access`. Full semantics: [`deploy-trunk.md`](./deploy-trunk.md) §§2–4.
- **`[oci]`**: `enabled` switch; the `/v2` surface is double-gated (storage-only + enabled), otherwise a bare 404. See [`refactoring/oci.md`](./refactoring/oci.md) and [`deploy-trunk.md`](./deploy-trunk.md) §10.
- **`[agent_capture]`**: `enabled` + `tenant_id` / `deployment_id` + `[[agent_capture.ingest_tokens]]` (name / token / paths). Storage-only only; enabling requires an explicit `git.push_auth` and at least one token. See [`refactoring/agent-capture.md`](./refactoring/agent-capture.md).
- **`[storage_events]`**: outbound events for committed writes (default off). `enabled` (requires storage-only), `installation_id` (required, never auto-generated), numeric bounds for concurrency / timeouts / graceful shutdown, `[[storage_events.targets]]` (id / https url / `secret_ref` HMAC / events / filter lists). Frozen validation rules: [`refactoring/config.md`](./refactoring/config.md) and [`refactoring/storage-events.md`](./refactoring/storage-events.md).
- **`[github_sync]`**: GitHub outbound sync (default off). `enabled`, `ssh_host` / `ssh_user` / `ssh_host_key`, `ssh_key_ref` (a SecretRef string — never put the private key itself here), four timeouts, `[[github_sync.bindings]]` (id / path / remote). See [`refactoring/github-sync.md`](./refactoring/github-sync.md).

## 5. Fail-closed startup validation

`Config::validate` / `AppContext::new` reject non-compliant configuration at startup instead of running degraded. The trunk / storage-only invariant checklist (`cedar.enforcement` must be off, no open CLs, non-terminal `push_queue` rows, explicit `push_auth` matching the morphology, `ssh_receive_pack = false`, and so on — 7 items) is authoritative in [`deploy-trunk.md`](./deploy-trunk.md) §1 and not repeated here. Other representative rejections: unknown / removed fields, invalid `cedar.enforcement` values, the reserved anonymous principal in `monorepo.admin`, `[oci]` / `[agent_capture]` / `[storage_events]` enabled outside storage-only, out-of-range `storage_events` numbers and non-canonical filters, SecretRef namespace mismatches. Error text follows the conventions in [`errors.md`](./errors.md) and never contains secret values.

## 6. `config validate`

Validate after editing, before starting a service:

```bash
mega2 --config /etc/mega2/config.toml config validate
# With a profile and source diagnostics
mega2 --config /etc/mega2/config.toml --profile prod \
  config validate --show-sources
# CI: warnings fail; secret refs are actually resolved (never printed)
mega2 --config /etc/mega2/config.toml \
  config validate --deny-warnings --resolve-secrets
```

- `--resolve-secrets`: bootstraps the Vault and resolves the config's SecretRefs, verifying reachability and field presence; resolved values are neither printed nor written back.
- `--deny-warnings`: exits non-zero when source diagnostics (ignored fields, env mapping problems, etc.) contain warnings.
- `--show-sources`: prints field provenance and override relationships across the base / profile / env layers; `--format json` emits the frozen JSON schema (including the winning source of `cedar.enforcement`, ADR-UN-01).
- `config init [--output <path>] [--force]`: writes the commented template config and prints follow-up `config secret set` and `config validate --resolve-secrets` guidance.

`config validate` uses read-only loading (§1): it never generates a default config, and a missing named file is an error. Configuration conventions for development and test environments: [`development.md`](./development.md). Repo-level conventions: [`AGENTS.md`](../AGENTS.md).

## 7. Related documents

- This documentation set: [`quick-start.md`](./quick-start.md) · [`user-guide.md`](./user-guide.md) · [`deployment.md`](./deployment.md) · [`architecture.md`](./architecture.md) · [`contributing.md`](./contributing.md)
