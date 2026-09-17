# Configuration

`config/config.toml` is the canonical sample configuration. Validate a selected
base/profile/environment combination before starting a service:

```bash
cargo run -p mega2 -- --config config/config.toml config validate
```

Configuration source diagnostics are available with `--show-sources`; use
`--deny-warnings` in automation that must reject ignored or deprecated input.
Secrets are supplied by deployment configuration or supported Vault
`SecretRef`s and must never be committed.

## Monorepo object-ID format

`[monorepo].object_format` selects the object-ID algorithm for an empty
Monorepo's initial graph and for live Git service. Canonical values are `sha1`
(the default), `sha256`, and `blake3`. The parser also accepts `sha-1` and
`sha-256`. `black3` is not a valid spelling or value. The setting is
restart-required and does not convert an existing repository.

`sha1` is the stock Git format. `sha256` and `blake3` are git-internal / Libra
extensions: normal service advertises `object-format={kind}` and pack
encode/decode uses `*_with_hash_kind`. Do not claim interoperability with stock
Git clients for those values (DEFER-B3-01). Cross-kind or wrong-width IDs and
pack trailer mismatches fail closed.

Git Object Format is independent of LFS Digest Algorithm
(`Git=blake3/LFS=sha256` and `Git=sha256/LFS=blake3` are expressible). LFS
BLAKE3 as a product transfer path is `DEFER-B3-LFS-01`.

Run `mega2 --config <path> service init --yes` to create the initial
graph; the command requires an existing config and exits without starting Git
listeners or normal service runtime dependencies. See
[`protocol.md`](./protocol.md) and [`plan/plan-20260907.md`](../plan/plan-20260907.md).

## Cedar authorization enforcement

The `[cedar]` section controls the authorization enforcement switch (ADR-UN-01):

```toml
[cedar]
enforcement = "off"   # off | shadow | enforce
```

- `off` (default): do not build or consume authorization data; zero behavior
  change.
- `shadow`: build the store, evaluate, record would-deny logs, but do not
  change allow decisions.
- `enforce`: build the store, evaluate, and deny unauthorized requests.

`config validate` rejects any other `enforcement` value, and rejects a
`monorepo.admin` entry equal to the reserved anonymous principal
`User::"__anonymous__"` (ADR-UN-06 ⑤). Source diagnostics for `cedar.enforcement`
are available via `config validate --show-sources --format json` (the JSON
output includes the field's winning source).

## Push morphology and trunk authentication

`[monorepo].push_policy` is `"review"` (default, CL pipeline) or `"trunk"`
(MonoWriteQueue). `[monorepo].max_push_commits` (default 250) bounds first-parent
chain length in trunk morphology only; review morphology keeps the
`MAX_CL_CHAIN_COMMITS` constant. Both fields are restart-required.

`[git].push_auth` omitted keeps the existing OAuth / UserStorage chain (review
only). Explicit `"token"` or `"none"` requires `push_policy = "trunk"`; trunk
requires an explicit `push_auth`. `[[git.push_tokens]]` entries authorize by
component-boundary path prefix (`/foo` does not authorize `/foobar`); omitted
`paths` means the whole repository. Token values may be literals (IT/tests) or
`vault://secret/config/<profile>/git/push_tokens/<name>#<field>` SecretRefs.
File-mounted secrets use `${file:...}` and are expanded at load.

Trunk HTTP start additionally refuses open change lists, a `push_policy`
change while `push_queue` has non-terminal rows (`queue_control.last_policy`),
and resets `blob_paths.indexed_push_id` to NULL on a successful empty-queue
morphology switch.

## Website authentication and product email

Browser sessions are validated against website Better Auth. The `[oauth]`
section supplies the website API base URL, accepted session-cookie names, and
the CORS allow-list. The complete trust boundary is documented in
[`website-auth.md`](./website-auth.md).

Product-email delivery is delegated to the website. Mega2 configuration
contains the website email API base URL and bearer/SecretRef used by the
notification client; see [`website-mail.md`](./website-mail.md) for the
request contract and Compose values.

There is no `[mail]` section, `MailConfig`, `mail.password` SecretRef, or
`MEGA_MAIL__*` environment override. These removed inputs are rejected rather
than silently ignored.

## Snowflake worker ID

`idgenerator` 2.0.0 allows at most 22 bits for `worker_id_bit_len + seq_bit_len`.
Mega's target layout is 8+8 (256 workers, timestamp shift 16). This process
keeps the existing 6+8 layout (64 workers, shift 14) because ADR-FC-04 forbids
mixing old and new writers, and this deploy cannot fence every previous
`worker_id(1)` process before the new bits go live.

Worker id is chosen once at startup, in order:

1. `MEGA_ID_GENERATOR_WORKER_ID` when it parses as an integer in `0..=63`.
   Invalid or out-of-range values log a warning (not the raw value) and fall
   through.
2. Redis `SET NX PX` on `mega2:snowflake:worker:<id>` with a 30s TTL.
   The owner refreshes with a compare-and-PEXPIRE token every 15s. Redis
   errors or a full 0..=63 map fall through.
3. Stable FNV-1a of `POD_UID`, else `HOSTNAME`, else `mega2-local`,
   reduced into `0..=63`.

ID generation after init does not talk to Redis. Logs include source
(`Env`/`Redis`/`Hash`), worker id, bit lengths, and process identity — never
the lease token, Redis URL, or pod secrets.

## Notification settings

Notification configuration controls mega2-owned delivery such as in-app,
Slack, and webhook notifications. The `email` preference requests delivery
through the website API; it does not configure a local mail provider. Refer to
[`notification.md`](./notification.md) for behavior and test boundaries.

## Storage-only outbound events

`[storage_events]` is a restart-required, default-disabled committed-write
emitter surface (plan-20260912 / WH-01). `enabled = true` requires
`git.storage_only()` (`push_auth` is `"token"` or `"none"`); review morphology
is rejected at `config validate`. `installation_id` is required when enabled
and is never auto-generated. Target HMAC values are `secret_ref` URIs; they
are not resolved while disabled.

Target `url` values must be HTTPS without userinfo, query, or fragment.
Numeric bounds hold even when the table is disabled: `max_in_flight` is
1..=64, `connect_timeout_seconds` is 1..=5, `request_timeout_seconds` is
1..=10, and `shutdown_grace_seconds` is 0..=10 (WH-14; `0` aborts in-flight
sends at once, and the ceiling is what keeps ADR-WH-03's drain bounded).

Filter entries validate per source dimension. `git_paths`, `lfs_paths` and
`agent_repo_paths` must be absolute canonical paths; `oci_repositories` must
be canonical lowercase distribution `remoteName` values, judged by the same
rule as the inbound `/v2` router (`src/common/oci_name.rs` is the single
implementation shared by both callers — WH-14). `agent_tenants` is compared
verbatim against the server-supplied value. A name the router would reject
can never match an inbound repository, so such a filter entry is a silently
dead subscription and is rejected rather than accepted. Every list is capped
at 64 entries of 1..=256 bytes each.

Every target `secret_ref` must use the namespace
`vault://secret/config/<profile>/storage_events/targets/<id>/hmac#<field>`,
where `<id>` is the target's own `id`; `config validate` enforces the shape
and namespace in both enabled and disabled modes (string-level only — it
never resolves). When enabled, service startup (WH-11) resolves each ref
through the vault and compiles the HMAC key: the resolved value must be
`hex:<even-hex>` decoding to 32..=256 bytes. Any resolution or encoding
failure fails startup with a redacted error; the resolved value is never
written back into the config snapshot or logged, and the SecretRef URI
stays out of error text. Secrets are seeded/rotated by piping the
`hex:<even-hex>` value on stdin (`--value-stdin` is required and is the only
way to supply the value), e.g.:

```bash
printf '%s' "$STORAGE_EVENTS_HMAC" | mega2 --config config/config.toml \
  config secret set storage_events.targets.ops-main.secret_ref \
  --vault-path config/prod/storage_events/targets/ops-main/hmac \
  --field value --value-stdin
```

Rotation uses `config secret rotate` with the same arguments and requires a
service restart (no hot reload of `[storage_events]`).

The HTTPS HMAC transport is wired into `AppContext` at startup (WH-11): on
enabled configs the resolved targets plus an `HttpsEventTransport` become the
single application emitter owner on `Storage`; on disabled configs nothing
is resolved and the emitter stays disabled. Each POST resolves DNS once,
rejects loopback/private/link-local/metadata/mixed results, pins the
connection to that verified address, and keeps the original hostname for TLS
SNI. All six source hooks are installed (WH-03..WH-08): `repo.push`,
`oci.manifest.published`, `lfs.object.uploaded`, `lfs.media.finalized`,
`agent_capture.events.committed` and `agent_capture.checkpoint.committed`.
See [`storage-events.md`](storage-events.md) for the per-source commit points
and the registered coverage gaps.

Target `events` must be unique literals from the six frozen types, at most 16
targets per table, and `agent_tenants` / `agent_repo_paths` are both empty or
both non-empty (an empty filter list means that source is not subscribed — it
is not a wildcard).

Upgrading from a release before the WH-14 gates: `shutdown_grace_seconds`
above 10 and non-canonical `oci_repositories` entries used to load, and now
fail `config validate` and service start. Both shipped samples keep
`[storage_events]` disabled with the targets commented out, so only
deployments that hand-wrote those values are affected; correct them before
upgrading.

## Test configuration

`.env.test.example` documents public test endpoints for PostgreSQL, Redis, and
RustFS. Its `MAILPIT_*` values are optional website-next SMTP-capture inputs;
mega2 neither reads them nor sends SMTP. The Compose topology and service
profiles are documented in [`test-infra.md`](./test-infra.md).
