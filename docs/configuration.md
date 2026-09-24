# Configuration Reference

English · [中文](configuration.zh.md)

This document is the operator's guide to the mega2 configuration system: load order, secret management, hot reload, a per-section tour, startup validation, and `config validate`. The fully commented per-key sample is authoritative in [`config/config.toml`](../config/config.toml) and is not duplicated here. Repository behavior is covered by the [User Guide](./user-guide.md); deployment steps are in the [Deployment Guide](./deployment.md).

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

- **SecretRef**: `vault://secret/<name>#<field>`, resolved through the embedded Vault (`libvault` crate + `src/contract/vault/`). `SecretRef`'s `Debug` / `Display` are always the redacted `vault://secret/***#***`; error text contains neither the real path nor the value.
- **File mounts**: `${file:/run/secrets/xxx}` expands to the file contents at load time (`src/config/expand.rs`). A value containing `${file:...}` must consist only of placeholders and literal text — it cannot be mixed with `${var}`; an unterminated `${file:` is rejected as well.

Only these config secrets can be stored in the mega2 Vault through `config secret` (see `src/commands/config.rs`): `redis.url`, `notification.webhook.token`, `object_storage.s3.access_key_id`, `object_storage.s3.secret_access_key`, and `storage_events.targets.<id>.secret_ref`. **Database credentials are deliberately excluded** because Vault needs them during bootstrap; keep them in deployment or environment secrets. Each field uses a fixed namespace such as `config/prod/redis/url`. `config validate` checks the SecretRef format but does not resolve it.

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
- **`[monorepo]`**: `import_dir` (default `/third-party`, the ImportRepo multi-branch exception), `admin`, `root_dirs` (directory initialization), `object_format` (`sha1` by default; `sha256` / `blake3` require Libra), `push_policy` (`review` by default or `trunk`), and `max_push_commits` (the trunk push-chain limit). User-visible path and push behavior is in the [User Guide](./user-guide.md); the full key list and defaults are in [`config.toml`](../config/config.toml).
  - **Path validation:** `config validate`, service startup, and hot-reload candidates use the same checks and report the invalid field. Each `root_dirs` entry must be a unique single path component: no `/`, `\`, or NUL; not empty, `.` or `..`; no leading or trailing spaces; and not a reserved root entry (`.cedar`, `.mega_cedar.json`, `.buckroot`, `.buckconfig`, or `.git`, case-insensitive). `import_dir` must be a canonical absolute non-root path with no trailing slash, `//`, `.` or `..` segments, NUL, or `\`; its first component must appear in `root_dirs`. Paths under `import_dir` always use ImportRepo semantics, even when `root_dirs` contains that name. `root_dirs` creates directories only during initial setup; changing it later requires a restart and does not add or remove directories in an initialized repository. See the [initialization manual](./manual/monorepo-init.md).
- **`[monorepo.rename]`**: move/rename detection for diff classification: `similarity_threshold` (0-100), `rename_limit` (0 = unlimited).
- **`[pack]`**: receive-pack decode resources. `pack_decode_mem_size` / `pack_decode_disk_size` (K/M/G, KiB/MiB, and percentages supported), `pack_decode_cache_path`, `clean_cache_after_decode`, `channel_message_size`, `save_entry_concurrency`.
- **`[lfs]`**: `[lfs.ssh].http_url` (href base for SSH transfer; LFS payloads still move over HTTP) and `[lfs.local].lfs_file_path`. In trunk mode, LFS authorization follows `git.push_auth`; see the [Deployment Guide](./deployment.md).
- **`[object_storage]`**: global backend for Git blobs, LFS, and artifacts. `storage_type` can be `local`, `s3`, `s3compatible`, or `gcs`; provider-specific settings live under `[object_storage.s3]`, `[object_storage.gcs]`, and `[object_storage.local]`. `config validate` rejects `vault://` values in S3 access keys; inject bootstrap credentials through the environment, a profile, or deployment secrets.
- **`[oauth]`**: website Better Auth session validation. `website_api_base_url`, `session_cookie_names` (omitted = Better Auth default cookie names), `allowed_cors_origins` (empty = built-in development defaults; env override is a comma-separated list). See the Architecture Guide for the service boundary.
- **`[blame]`**: large-file thresholds and traversal resources: `max_lines_threshold`, `max_size_threshold`, `default_chunk_size`, `max_commits_in_memory`, `enable_caching`.
- **`[redis]`**: `url`. Cache / distributed locks / snowflake worker leases, etc.; may be Vault-backed via SecretRef.
- **`[buck]`**: Buck upload API: session and file limits (`session_timeout` / `max_file_size` / `max_files` / `max_concurrent_uploads`), server-side rate limiting (`upload_concurrency_limit` / `large_file_concurrency_limit` / `large_file_threshold`), session cleanup task (`enable_session_cleanup` / `cleanup_interval` / `completed_retention_days`, partially hot-reloadable — see §3).
- **`[artifacts_gc]`**: GC of unreferenced artifact blobs (`enable` / `interval_secs` / `grace_secs` / `batch_limit`), default off; tuning hot-reloads, enabling from false requires a restart.
- **`[notification]`**: `enabled` global kill switch (hot-reloadable); optional `[notification.webhook]` outbound channel (`url` is non-secret, `token_ref` is a SecretRef). The sample config documents every supported field.
- **`[vault.audit]`**: see §2.
- **`[cedar]`**: `enforcement = "off" | "shadow" | "enforce"` (ADR-UN-01; default off). Trunk mode requires `off`. The Architecture Guide summarizes authorization behavior.
- **`[git]`**: authentication for the Git protocol and product API writes. `push_auth` (omitted = OAuth/UserStorage chain, review only; `"token"` / `"none"` require trunk), `[[git.push_tokens]]` (name / token / paths, component-boundary prefix authorization; tokens via `${file:...}` or SecretRef), `ssh_receive_pack` (storage-only must set it to `false` explicitly), `anonymous_access`. See the [Deployment Guide](./deployment.md) for deployment practices.
- **`[oci]`**: `enabled` switch; the `/v2` surface is double-gated (storage-only + enabled), otherwise a bare 404. See the [User Guide](./user-guide.md) for product surfaces.
- **`[agent_capture]`**: optional Agent Session Capture API for sessions, events, checkpoints, transcripts, and file-operation records; it is independent of Git push. Configure `enabled`, `tenant_id` / `deployment_id`, and `[[agent_capture.ingest_tokens]]` (name / token / paths). Available only in storage-only mode; enabling requires explicit `git.push_auth` and at least one separate ingest token. See the [Agent Capture configuration and API reference](./refactoring/agent-capture.md).
- **`[storage_events]`**: outbound events for committed writes (default off). `enabled` (requires storage-only), `installation_id` (required, never auto-generated), numeric bounds for concurrency / timeouts / graceful shutdown, `[[storage_events.targets]]` (id / https url / `secret_ref` HMAC / events / filter lists). The commented sample and `config validate` describe supported values and validation.
- **`[github_sync]`**: GitHub outbound sync (default off). `enabled`, `ssh_host` / `ssh_user` / `ssh_host_key`, `ssh_key_ref` (a SecretRef string — never put the private key itself here), four timeouts, `[[github_sync.bindings]]` (id / path / remote). See [`config.toml`](../config/config.toml) for the full schema.

## 5. Fail-closed startup validation

`Config::validate` and `AppContext::new` reject invalid configuration at startup. The [Deployment Guide](./deployment.md) covers trunk / storage-only prerequisites, including `cedar.enforcement = "off"`, no open CLs, no non-terminal `push_queue` rows, an explicit `push_auth` compatible with the selected mode, and `ssh_receive_pack = false`. Other examples include unknown or removed fields, invalid `cedar.enforcement` values, the reserved anonymous principal in `monorepo.admin`, optional services enabled outside storage-only mode, out-of-range `storage_events` values, non-canonical filters, and SecretRef namespace mismatches. Secret values are never included in error messages.

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

`config validate` uses read-only loading (§1): it never generates a default config, and a missing named file is an error. Configuration conventions for development and test environments are in the [Contributing Guide](./contributing.md). Repo-level conventions: [`AGENTS.md`](../AGENTS.md).

## 7. Related documents

- This documentation set: [`quick-start.md`](./quick-start.md) · [`user-guide.md`](./user-guide.md) · [`deployment.md`](./deployment.md) · [`architecture.md`](./architecture.md) · [`contributing.md`](./contributing.md)
